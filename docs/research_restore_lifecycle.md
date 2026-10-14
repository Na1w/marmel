# Shutdown / restart lifecycle + deleted restore prior-art (t: rehydration design input)

Baseline: HEAD `bbfb893` (`1.0.0-refactor`), read-only. Persistence *content* is covered by
`docs/research_persist_manager.md` / `docs/research_persist_ui.md` — referenced, not repeated.
Every claim below was read from source or `git show`; line numbers are from this working tree.

## 0. Answer summary

There is **no shutdown-time state capture at all** in the current binary: the only shutdown work is
terminal restoration (`src/main.rs:279` → `ui::restore()`; `src/ui/tui/mod.rs:947-955` + `Drop` at
`:1052-1056`), plus `orchestrator::cancel_all()` + `rt.shutdown_timeout(300 ms)` at `src/main.rs:64-65`,
which are **skipped entirely on panic and on any signal** (there is no signal handler anywhere in `src/`:
0 hits for `tokio::signal` / `sigaction` / `signal(`; `tokio` has `features = ["full"]`, `Cargo.toml:14`).
What survives a restart is exactly what the loop happened to have flushed incrementally: the manager
transcript (`ctx.save_transcript`, 22 call sites, all in `src/ui/session.rs`, plain `fs::write`) and the
UI transcript (`ui_transcript.save`, 22 call sites, plain `fs::write`); **neither is ever *read back*** —
`ContextEngine::load_transcript` (`src/manager/context.rs:998`) and `UiTranscript::load`
(`src/ui/transcript.rs:89`) are dead in production (`UiTranscript::load` only in a unit test,
`src/ui/transcript.rs:335`), and boot always starts from `UiTranscript::new()` (`src/ui/session.rs:302`)
and a fresh `build_manager_context` (`src/ui/session.rs:280`). The deleted prior art
(`src/orchestrator/freeze.rs`, `src/ui/session/recovery.rs`, removed in `974b601 "Refactor part 1"`,
recovered from `974b601^` = `31f4e90`) never aimed at EXACT restore either: its durable model was
`FreezeSnapshot { worker_id, agent_name, sub_req }` → `.session_frozen.json` + an append-only
`.session_journal.json`; `recover_frozen()` **re-ran** the worker from the *initial* request
(`31f4e90:src/orchestrator/mod.rs:432` `IsolatedContext::from_request(prompt, &snap.sub_req)`,
`:471` `worker.run(&ctx, &child_token).await`) with zero accumulated history, `frozen()` returned only
the **first** snapshot (`freeze.rs:134` `list.into_iter().next()`), subagent lists were *heuristically
reconstructed* from 7 artifacts (`31f4e90:src/ui/helpers.rs:505-520`), and the UI was re-**rendered**
from `UiRecord`s with synthesized prefixes (`31f4e90:src/ui/tui/mod.rs:1086+`), not restored byte-exact.
For exact rehydration you need (a) a synchronous flush of both transcripts at every break path
(including a real SIGTERM/SIGHUP handler and a snapshot inside the panic hook at `src/main.rs:277-281`),
(b) a new per-worker conversation journal — the specialist's `engine` is a **local variable** inside
`run_specialist_live_inner` (`src/agents/runner/execution.rs:175`, engine built ~`:250`) and is never
reachable or persisted, and (c) a UI-model snapshot, because `UiRecord` has no variant for streamed
subagent text (`src/ui/transcript.rs:14-40`) and no TUI pane state is journaled.

---

## 1. Exit-path inventory (path:line → what runs → state alive at that moment)

### 1.1 `src/main.rs` (404 lines, all read)

| Path:line | What runs | State that exists at that moment |
|---|---|---|
| `main.rs:18` `fn main() -> Result<()>` | entry | none |
| `main.rs:19-22`, `:88-92` | Landlock re-entry sentinel short-circuit | none (separate process path) |
| `main.rs:132`, `:173`, `:183` | `std::process::exit(...)` (sandbox refuse / exec fail / non-unix wait) | no session; **no cleanup** |
| `main.rs:24`, `:196-233` | `parse_args()` (hand-rolled; see 1.1.1) | argv only |
| `main.rs:29`, `:33` | `config::load(args.config)` (`src/config.rs:311`), `config::set_active` (`src/config.rs:202`) | config; `Plan::default()` dir = `.marmel` (`src/manager/phase.rs:269-273`) |
| `main.rs:43-44`, `:242-275` | `use_raw` decision; `setup_panic_hook(use_raw)` also installs the tracing subscriber | log file opened/rotated (`:249-250`, `:255-262`) |
| `main.rs:276-282` | `std::panic::set_hook` (`:277`); body = `catch_unwind` (`:278`) → `ui::restore()` (`:279`) → `default_hook(info)` (`:281`) | **only terminal restore**; `ui::restore()` = `tui::leave_alt_screen()` + `raw::restore()` (`src/ui/mod.rs:132-135`, `src/ui/tui/mod.rs:1059-1067`, `src/ui/raw.rs:31-40`). No flush, no save. |
| `main.rs:46-48`, `:50-54`, `:56` | multi-thread runtime; `mcp::McpManager::boot` → `harness::set_mcp_manager(Arc)`; `boot_manager(&cfg)` (`:187-195`: `Plan::default()`, `HarnessStats`, `ChatClient::from_config`, `OrchestratorManager::from_config`) | global singletons live: MCP manager, active config, `GLOBAL_CANCELLATION_TOKEN` (`src/orchestrator/bus.rs:16`) |
| `main.rs:58-62` | `rt.block_on(ui::raw::run(...))` / `rt.block_on(ui::tui::run(...))` — the whole session; a panic here unwinds `main` | session state as below |
| `main.rs:64-65` | `orchestrator::cancel_all()` (global token + `cancel_all_active_workers()`, `src/orchestrator/bus.rs:38-43`), then `rt.shutdown_timeout(300 ms)` | **grace window = 300 ms**; a Drop-based flush inside a task is not reliable after this |
| `main.rs:66` | `res` returned → exit code | `rt` already shut down |
| panic path | hook runs, then unwind; `main.rs:64-65` **skipped**; runtime `Drop` during unwind blocks on workers | last in-loop save on disk; all in-memory registries gone |
| SIGTERM / SIGKILL / SIGINT (headless) / crash | **default disposition — nothing runs** (`grep -rn "tokio::signal\|sigaction\|signal(" src/` → 0 hits) | last in-loop save on disk; PTY children **orphaned** |

#### 1.1.1 `CliArgs` / `parse_args` (for adding `--resume`)
- `struct CliArgs` `main.rs:9-16` (`config`, `raw`, `debug`, `prompt`), `#[derive(Debug, Default)]` — add `resume: bool` at `:16`.
- `parse_args` `main.rs:196-233`: `while let Some(arg) = it.next()` + `match arg.as_str()`; arms: `--config` `:201-208` (missing value → `exit(2)` `:206`), `--raw` `:209`, `--debug` `:210`, `-h|--help` `:211-222` (`exit(0)` `:221`), fallback `other` `:223-229` → first positional becomes `prompt`, later ones print `ignoring extra argument` `:227`.
- Consequence: a `--resume` arm is a one-liner next to `:210`, but any **unknown** flag is currently swallowed as a positional prompt (`:224-226`) — so `--resume` must be added *before* the `other` arm or it silently becomes the goal. Help text block `:213-220` also needs the flag.
- Plumbing seam: `main.rs:59-61` → `ui::raw::run` (`src/ui/raw.rs:20-27`) / `ui::tui::run` (`src/ui/tui/mod.rs:1070-1077`) → both call `super::run_session(cfg, &mut renderer, initial, manager)` (`src/ui/raw.rs:26`, `src/ui/tui/mod.rs:1076`) → `run_session` `src/ui/session.rs:220-227` → `run_session_with_bounds` `:241-252` → `run_session_with_bounds_report` `:258-264`. Four signatures to touch (or put the flag in `Config`).
- Precedent deleted: the config key `enable_rehydration` (`31f4e90:src/config.rs:128-129, 155, 198-200, 261, 342-344`) is gone — `grep enable_rehydration src/config.rs` → 0 hits; `PartialConfig` has no `deny_unknown_fields`, so old TOML keys are ignored silently.

### 1.2 `src/ui/raw.rs` (headless — **raw mode is never entered here**)
- Module docs `:1-6`: "never enters raw mode or the alternate screen". `RawRenderer::init` `:92-95` = no-op; `flush` `:139-148` writes the line buffer to stdout; `shutdown` `:169-171` = `flush()` only; **no `Drop` impl for `RawRenderer`**.
- `restore()` `:31-40` = defensive `disable_raw_mode()` (panic-hook parity).
- `request_user_exit` `:163-166` sets `aborted`+`user_exit` **and calls `crate::orchestrator::cancel_all()`** — the headless abort is therefore a *hard* cancel.
- Signal behaviour: with `--raw` or piped stdout (`main.rs:43`) the process keeps default `ISIG`, so Ctrl-C = SIGINT → instant death, no flush, no shutdown.

### 1.3 TUI raw-mode / alt-screen (the real one: `src/ui/tui/mod.rs`)
- `init` `:406-415`: `enable_raw_mode()` `:407`, `EnterAlternateScreen` + `PushKeyboardEnhancementFlags` `:408-412`, `Terminal::new` `:414-415`, then **the only boot rehydration that exists today**: plan file read into `plan_content` / `show_plan_panel` / `had_active_plan` `:416-424`; `force_flush()` `:426`.
- Raw mode clears `ISIG` ⇒ in TUI mode Ctrl-C is a **key event**: `src/ui/tui/events.rs:154-166` (first press arms `confirm_abort` + `abort_armed_at`, second press → `Renderer::request_user_exit`), Ctrl+D `:167-170`, Esc `:44-59`; `request_abort` `:921`, `request_user_exit` `:928-931`, `clear_abort` `:933-944` (also `orchestrator::reset_cancellation()` `:944`).
- Teardown: `shutdown` `:947-955` (`PopKeyboardEnhancementFlags`, `LeaveAlternateScreen`, `DisableMouseCapture`, `disable_raw_mode`, all best-effort `let _ =`); **Restore-on-Drop guard = `impl Drop for TuiRenderer` `:1052-1056` → `shutdown()`**; panic-hook path uses `leave_alt_screen()` `:1059-1067`.
- Full `impl Drop` inventory in `src/` (non-test): `orchestrator/workers.rs:154`, `orchestrator/preemption.rs:208`, `harness/fs.rs:454`, `harness/pty.rs:344`, `:867`, `:1127`, `ui/tui/mod.rs:1052`. **None persists session state.**

### 1.4 `src/ui/session.rs` (2430 lines) — abort breaks and what is already appended
Boot/order: `:265` `clear_abort()`, `:266` `renderer.init()`, `:276` system prompt, `:280` `ctx = build_manager_context(...)` (def `:182`), `:297-299` register `status_tx`/`event_tx`/`steering_history` globals, `:300-301` `ui_transcript_path`/`transcript_path`, `:302` **`UiTranscript::new()` (never loaded)**, `:319` **`subagents = Vec::new()` (never restored)**, `:381-382` goal + `UiRecord::User` + `save`, `:389` `reset_cancellation()`, `:391` `while !renderer.aborted()`.

Exit / break paths and their persisted suffix:

| Path:line | What runs | Persisted at that moment |
|---|---|---|
| `:374-377` | `read_input()` `None` at goal prompt → `renderer.shutdown()` + `return Ok(SessionStopReport::default())` | nothing but a plan file |
| `:425-428` | mid-loop `/abort` → `request_user_exit(); break` | falls through to tail `:1971` |
| `:431-439` / `:1107-1113` / `:1618-1626` / `:1944-1952` | `/reset` → `handle_reset_command` (`src/ui/helpers.rs:197-218` → `Plan::clear` deletes plan **and both transcripts**, `src/manager/phase.rs:407-421`) + `ui_transcript.clear()` + `remove_file(ui_transcript_path)` + `continue` | **history deliberately destroyed** |
| `:516` | `aborted \|\| user_exit_requested` → break turn | last in-loop save |
| `:627-635` | LLM wall-clock bound → `cancel_all()` + status + `break` | assistant message **not yet appended** (partial stream lost) |
| `:666-688` + `:689-690` | `tool_calls` extracted from `Message::Assistant`, then `ctx.append(assistant)` **and `ctx.save_transcript`** | the *whole* `tool_calls` batch is already on disk **before any dispatch** |
| `:691-707` | `UiRecord::Assistant` + `ui_transcript.save` | assistant text/thinking durable |
| `:715`, `:726`, `:956`, `:1224`, `:1272`, `:1371`, `:1407-1485`, `:1734`, `:1810-1932` | compaction / rebirth / tool-result / notice saves | incremental |
| `:784-934` | parallel fan-out: `handles: Vec<JoinHandle<_>>` via **`tokio::task::spawn_blocking`** `:860`, `:884`, `:913`, `:934` | `UiRecord::ToolCall` saved per non-delegate call `:842-844` |
| `:936-958` | flush error right after fan-out → `cancel_all()`, `handle.abort()` all, `ctx.ensure_tool_call_pairs()` + `save_transcript`, **`return Err(e)`** | repaired ctx on disk; UI transcript not; `main.rs:64-65` still runs |
| `:963-1005` | join loop (indexed so the unjoined tail stays reachable) | per-iteration saves |
| `:1007-1017` | `user_exit_requested` inside join → `cancel_all()` + synthetic `Err("aborted by user")` | see `:1221-1224` |
| `:1019-1033` | `aborted \|\| steer_abort_requested \|\| is_globally_cancelled` → `cancel_all()` + `timeout(500 ms, &mut handle)` (the task may still run) + `Err("aborted")` | same |
| `:1036-1052` | watchdog bound → `cancel_all()` + `bound_stop_reason` + `Err(reason)` | same |
| `:1221-1224` | `ctx.append(Message::Tool { … })` + `save_transcript` → aborted rounds **do** get a tool result | durable |
| `:1229-1232` | abort/cancel results excluded from the failure budget | — |
| `:1262-1274` | flush error after a join → `cancel_all()`, abort tail handles, repair + save, **`return Err(e)`** | repaired ctx only |
| `:1284-1286` | end-of-round `for handle in handles { handle.abort() }` | note: **`JoinHandle::abort()` on `spawn_blocking` cannot interrupt the closure** — only the `CancellationToken` does |
| `:1293`, `:1534-1560`, `:1637-1643` | sequential-dispatch abort/cancel sites | — |
| `:1766-1768` | break on user exit | — |
| `:1788-1794` | **`ensure_pairing_at_boundary("after tool round")`** (def `:2155-2176`): synthesizes `Message::Tool { content: ABORTED_TOOL_RESULT }` (`= "(aborted)"`, `src/manager/context.rs:327`) for the skipped tail, saves **both** transcripts, appends a `UiRecord::Status` | repaired, self-consistent pair of files |
| `:1797-1804` | `bound_stop_reason` / user-exit breaks out of the turn loop | — |
| `:1806-1825` | post-round compaction / rebirth saves | durable |
| `:1897-1923` | `steer_abort_requested` branch: `clear_abort()`, `reset_cancellation()`, mark active subagents inactive + `logs.push("[aborted by user]")`, `save_transcript` `:1916`, `continue`; else `break` | steering abort is **not** an exit |
| `:1938-1941` | `/abort` at `read_input` → `request_user_exit(); break` | last in-loop save |
| `:1971-1977` | **session tail**: `Event::Done`, `renderer.flush().ok()`, `renderer.shutdown()`, `Ok(SessionStopReport{stops})` | **no final `save_transcript`, no final `ui_transcript.save`, no snapshot of `subagents`** |

`ui_transcript.save` call sites in `src/ui/session.rs` (22): `:382, 400, 445, 707, 844, 985, 1078, 1204, 1211, 1249, 1347, 1370, 1511, 1590, 1712, 1719, 1759, 1874, 1960, 2118, 2173, 2207` (helpers `surface_plan_warning` `:2118`, `ensure_pairing_at_boundary` `:2173`, `surface_compaction_outcome` `:2207`). `ctx.save_transcript` call sites (22): `:690, 717, 727, 956, 1224, 1272, 1371, 1407, 1413, 1422, 1441, 1460, 1485, 1734, 1810, 1820, 1848, 1884, 1916, 1932, 1958, 2166`.

### 1.5 Worker/task teardown: `src/ui/bridge/`, `src/orchestrator/*` (no `join` of delegation tasks)
- `src/ui/bridge/steer.rs:84-93` `spawn_steer_arbitration(...) -> tokio::task::JoinHandle<()>` (spawn at `:108`). Every call site in `src/ui/session.rs` (`:447-455`, `:1113-1123`, `:1618-1628`) **discards the handle** → arbitrator tasks are detached, never joined, never aborted; only `steer_arb_rx` is drained (`src/ui/bridge/drain.rs:8`, `:25`). They die with `rt.shutdown_timeout` (`main.rs:65`).
- Manager tool rounds are `spawn_blocking` closures (`session.rs:860/884/913/934`) running `crate::harness::dispatch_for(...)`; `delegate_task` is dispatched at `src/harness/mod.rs:333/571/691/927/1079` → `orchestrator::handle_delegate_task` (`src/orchestrator/delegate.rs:31`), which runs the async delegation on a **separate OS thread** via `std::thread::scope` + `handle.block_on` (`delegate.rs:101-141`, with `catch_unwind` `:111-122` turning a worker panic into a `Failed` deliverable) or `futures::executor::block_on` (`:143`). `src/harness/mod.rs:291-312` `block_on_safe` = `block_in_place` on a multi-thread runtime. Consequence: `handle.abort()` detaches the outer handle only; the worker future is stopped **cooperatively only**.
- Cancellation mechanism: `src/orchestrator/bus.rs:16-18` `GLOBAL_CANCELLATION_TOKEN: LazyLock<RwLock<CancellationToken>>`; `:33-35` task-local `CURRENT_WORKER_TOKEN`; `cancel_all()` `:38-43` (global `token.cancel()` + `cancel_all_active_workers()`); `is_globally_cancelled` `:46-51`; `reset_cancellation` `:64-68`. Orchestrator per-manager child token: `src/orchestrator/mod.rs:113`, `:141`, `:298`; abort short-circuit `:309-331`.
- Live worker table (**not** `src/orchestrator/registry.rs` — that file is the static *role* registry, `:28-38`, `canonical()` `:35-53`): `src/orchestrator/workers.rs:106` `static WORKERS: LazyLock<DashMap<String, WorkerState>>`; `ActiveWorkerInfo` `:28-41`, `WorkerState` `:61-77`, `is_active()` `:78`, completion ring cap `MAX_RECENT_COMPLETED = 10` `:100`; `ActiveWorkerGuard` + `Drop` `:152-208` (flips entry to completed, evicts ring, `notice::reclaim_worker_inbox`); keys `{agent}-{task}` or `{agent}-wN` `:229-260`; targeted cancel `:781-802`; `cancel_all_active_workers` `:805-824`.
- `src/orchestrator/preemption.rs`: `PauseSignal` `:26`, `StreamIdentity { cancel_token }` `:34-38`, `ACTIVE_STREAMS: LazyLock<RwLock<HashMap<u64, StreamEntry>>>` `:114`, `register`/`register_full` `:138-172`, `Drop` removes from the map `:208-214`, `emit` forwards `SubagentMessage`/`SubagentThinking` as UI events `:216-238`, `poll_control` yields `StreamControl` `:243+`, `PreemptHandle::complete_all` cancels tokens `:288-305`, `complete_with_subtask_decision` `:307-345`, `PREEMPT_ACK_TIMEOUT = 2 s` `:357`, `preempt_conflicting_stream` `:401-490` (non-acknowledging streams get `token.cancel()` `:472-486`).
- In-flight **steering notices are volatile**: `src/orchestrator/notice.rs:76` `PENDING_NOTICES`, `:81` `WORKER_INBOXES`, `:84` `REPLIED_NOTICES` — all `LazyLock<DashMap<_>>`, nothing on disk.
- PTY children: only cooperative kill — `src/harness/pty.rs:1560-1570` `kill_process_group(pid) = kill(-pid, SIGKILL)` (single unix site, called `:262-266`); **no `Pdeathsig`/`death_signal` anywhere** (`grep` → 0 hits), so a hard kill leaves tool child processes running.

---

## 2. Prior-art teardown (recovered from git, not guessed)

Recovery commands (verified): `git log --diff-filter=D --name-only -- '*freeze.rs' '*recovery.rs'` →
**`974b601 "Refactor part 1"`** deleted `src/orchestrator/freeze.rs` (371 lines) and
`src/ui/session/recovery.rs` (81 lines). Both are therefore readable at `974b601^` = **`31f4e90`**
(`fix(ui): disable session rehydration by default and prevent exit on user steering abort`):
`git show 31f4e90:src/orchestrator/freeze.rs`, `git show 31f4e90:src/ui/session/recovery.rs`.
Cross-check: `docs/cleanup_report.md:41-42, 51-52, 256, 311, 331, 358`. Current HEAD: `grep -rn rehydrate src/` → **0 hits** (feature fully removed).

### 2.1 Serialized data model (`31f4e90:src/orchestrator/freeze.rs`)
| Item | Line | Shape |
|---|---|---|
| `FROZEN_STATE_FILE` | `:25` | `.session_frozen.json` (in-flight checkpoint) |
| `CRASH_JOURNAL_FILE` | `:27` | `.session_journal.json` (append-only JSONL) |
| `FreezeSnapshot` | `:32-40` | `{ worker_id: String (uuid), agent_name: Agent, sub_req: DelegationRequest }` — **only the initial request** (`agent_name`, `prompt`, `snippets`, `task_id`, `image_urls`, `audio_urls`, `recursion_granted`) |
| `JournalEventKind` | `:45-51` | `Frozen \| Resolved \| Failed` |
| `JournalEvent` | `:56-61` | `{ ts RFC3339, kind, worker_id, agent, task_id }` |
| `JOURNAL_MUTEX` | `:64-65` | process-global `Mutex<()>`; every op takes it with `unwrap_or_else(PoisonError::into_inner)` |
| `RawFrozen` | `:69-71` | `#[serde(untagged)] Single(FreezeSnapshot) \| List(Vec<_>)` (format drift tolerance) |
| `CrashJournal { dir }` | `:77-79`, `Default` `:259-262` → `Plan::MARMEL_DIR` | file-backed |
| `snapshot()` | `:105-127` | mints a **new** `worker_id`, appends to the list, rewrites the whole file, appends a `Frozen` event |
| `frozen()` | `:129-135` | `Ok(list.into_iter().next())` → **first snapshot only** |
| `frozen_all()` | `:138-143` | all (used only by the heuristic UI reconstruction) |
| `clear(worker_id, resolved)` | `:150-176` | removes that entry, logs `Resolved`/`Failed`; unknown id → no-op + `Agent::Coder` fallback |
| `journal()` / `is_frozen()` | `:179-192` / `:195-202` | read JSONL / non-empty check |
| `read_frozen_list()` | `:209-229` | parse failure → `tracing::warn!` + **`Ok(Vec::new())`** (fail-open: a corrupt checkpoint silently means "nothing frozen") |
| `write_frozen_list()` | `:231-243` | plain `fs::write` (`to_string_pretty`), `remove_file` when empty, single-vs-array shape switch — **not atomic** |
| `append()` | `:245-255` | `OpenOptions { create, append }` + `writeln!` |

### 2.2 Snapshot triggers (`31f4e90:src/orchestrator/mod.rs`)
- `pub mod freeze;` `:16`; re-export `:58`; field `pub journal: CrashJournal` `:102`; constructed from the plan dir `:139`, `:148`.
- **Trigger 1 (only one):** `OrchestratorManager::delegate` writes the snapshot immediately **before** the worker runs: `:245-254` (`self.journal.snapshot(entry.agent, &req)`; on failure → `warn!` + `String::new()` and the task continues unprotected).
- **Trigger 2 (release):** clean finish `:332-335` `journal.clear(&worker_id, true)`; recovery failure `:391` `clear(..., false)`; recovery finish `:487` `clear(&snap.worker_id, clean)`.
- Nothing snapshotting manager context, UI state, worker progress, notices, PTY state, or plan state (plan/transcript files were separate, pre-existing).

### 2.3 Restore wiring at boot (`31f4e90:src/ui/session.rs`, all gated on `cfg.enable_rehydration`)
| Line | Action |
|---|---|
| `:56` | `if cfg.enable_rehydration {` … (default **false** since `31f4e90`; `31f4e90:src/config.rs:155` `enable_rehydration: false`, env `MARMEL_ENABLE_REHYDRATION` `:198-200`, TOML key `:261`/merge `:342-344`) |
| `:58-66` | `UiTranscript::load(&ui_transcript_path)`; on error → `UiTranscript::new()` |
| `:69-78` | `ctx.load_transcript(&transcript_path)` then `ctx.set_system_prompt(system)` (re-pin the freshly built system prompt) |
| `:81-90` | `renderer.rehydrate_ui(ut.records())`; fallback `ut = UiTranscript::from_legacy_messages(ctx.messages())` + `renderer.rehydrate_messages(...)` + `ut.save(...)` |
| `:94-102` | `ui_transcript.extract_steering_history()` → overwrite `steering_history` RwLock; status "Session transcript rehydrated from disk (Ready)" |
| `:113-127` | `subagents = rehydrate_subagents_with_ui(ctx.messages(), Some(&m.journal), None, Some(&plan), Some(&ui_transcript))` + `renderer.rehydrate_subagents(&sa)` |
| `:135-154` | frozen-recovery `while` loop: `mgr.journal.is_frozen()` → `tokio::spawn(recover_mgr.recover_frozen())`, 50 ms poll/drain loop, abort via `cancel_all()`, steering injection |
| `31f4e90:src/ui/helpers.rs:61-71` | when rehydration is on, the on-disk plan is injected into the **system prompt** ("There is an existing execution plan already active…") |
| `31f4e90:src/ui/mod.rs:129-134` | `Renderer::rehydrate_ui` / `rehydrate_messages` / `rehydrate_subagents` default methods; TUI impls `31f4e90:src/ui/tui/mod.rs:1074` (`rehydrate_subagents`) and `:1086` (`rehydrate_ui`) — **removed in `974b601`** (`docs/cleanup_report.md:358`: `src/ui/mod.rs −6`) |
| `31f4e90:src/ui/session/recovery.rs` | 81-line duplicate `recover_frozen_deliverable()` — **never compiled**: `git grep "mod recovery" 31f4e90 -- src` → no match (only `pub mod freeze;` existed). Live path was the inline loop in `session.rs`. |

### 2.4 Why it fell short of EXACT restore
1. **Worker history was not captured, and recovery re-ran from scratch.** `FreezeSnapshot` holds only `sub_req` (the *initial* brief). `recover_frozen()` rebuilds a fresh `IsolatedContext::from_request(prompt, &snap.sub_req)` (`:432`), re-synthesizes a blueprint if the `.marmel/prompts/<task>.md` file is missing (`:421-430`), and calls `worker.run(&ctx, &child_token).await` (`:471`) — turns already spent, tool calls, tool results, validator rounds and partial files are gone. `worker_id` is only used for journal bookkeeping (`:454` re-registers under a *new* registry key).
2. **Only one of N in-flight workers resumed.** `frozen()` returns `list.into_iter().next()` (`freeze.rs:134`); `frozen_all()` fed only the UI heuristic, never the recovery loop.
3. **Subagent lists were inferred, not restored.** `31f4e90:src/ui/helpers.rs:505-520` documents 7 inference sources (frozen file, transcript messages, journal events, UI records, recovered deliverable, plan boxes, `.marmel/prompts/*.md`), and `is_subagent_in_flight` `:439-497` decides `is_active` by matching task ids across three files + "Frozen event without later Resolved/Failed" + plan pending. Names were *synthesized* (`format!("{}-{tid}", …)`), `logs`/`thinking` were not recoverable at all.
4. **UI text was re-rendered, not replayed.** `rehydrate_ui` re-emits synthesized lines (`"User: {text}"`, `"[Tool Call] {display}"`, `"[{tid}] completed."`, `"[{tid} failed: r]"`, …) — the live path produces those bytes through different code (`src/ui/tui/mod.rs:585` `"[Tool Call] {text}"`, `:618`, `:765-769`), so exact byte equality was never the contract. Streamed assistant `current_content`/`current_thought`, and **all** `SubagentMessage`/`SubagentThinking` text (`src/orchestrator/preemption.rs:216-238`) have no `UiRecord` variant (`src/ui/transcript.rs:14-40`) → lost. Pane state (textarea draft `input_text`/`cursor`, `chat_scroll`, `plan_scroll`, `subagent_scroll`, `show_*_panel`, `focused_panel`, `history`, `status_line`, `frame_counter` — `src/ui/tui/mod.rs:40-122`) was never in the model.
5. **Fail-open on corruption:** `read_frozen_list` swallows parse errors into an empty list (`freeze.rs:218-229`); `load_transcript` refuses `< 2` messages and any parse error (`src/manager/context.rs:1003-1015`), `UiTranscript::load` error → `UiTranscript::new()`. Any of these silently degrades to a fresh session.
6. **No shutdown-side write:** nothing in the exit paths (panic hook, `main.rs:64-65`, Drop) wrote a snapshot; the journal only ever described "a delegation started", never "where it stopped".
7. **Both writers non-atomic** (`fs::write` / `append`) — a crash mid-write yields a truncated file, which then hits rule 5 and loses the whole session.
8. **Feature off by default and unwired:** `enable_rehydration: false` (`31f4e90:src/config.rs:155`), and `recovery.rs` was dead code — the shipped behaviour was "start fresh".

---

## 3. Boot-path insertion points for `--resume`

Current boot order (all verified): `main.rs:24` `parse_args` → `:29` `config::load` (`src/config.rs:311`) → `:33` `set_active` → `:43-44` `use_raw`/panic hook → `:46-48` runtime → `:50-54` MCP boot → `:56` `boot_manager` (`:187-195`) → `:58-62` `block_on(run)` → `ui::raw::run` (`raw.rs:20-27`) / `ui::tui::run` (`tui/mod.rs:1070-1077`) → `run_session` (`session.rs:220`) → `run_session_with_bounds_report` (`:258`) → `:265-319` session bootstrap → `:391` loop.

Live vs dead at boot:

| Item | Boot status |
|---|---|
| `config::load` + `set_active` | **live** (`main.rs:29`, `:33`) |
| MCP boot | **live** (`main.rs:50-54`) |
| `boot_manager` → `Plan::default()` (dir `.marmel`), registry `canonical()`, `cancellation_token = global.child_token()` | **live** (`main.rs:187-195`, `orchestrator/mod.rs:141`) |
| Plan file → renderer pane | **live** (`tui/mod.rs:416-424`) — the only rehydration today |
| `MessageManager::load_transcript` → actually `ContextEngine::load_transcript` (`manager/context.rs:998`) | **DEAD in production** — only reference is a doc comment (`context.rs:268`); no call site in `src/` |
| `UiTranscript::load` (`ui/transcript.rs:89`) | **DEAD** — only `ui/transcript.rs:335` (unit test) |
| `TRANSCRIPT_FILE` = `.session_transcript.json` (`manager/phase.rs:117`), `UI_TRANSCRIPT_FILE` = `.ui_transcript.json` (`:119`), `Plan::transcript_path()` `:298`, `Plan::ui_transcript_path()` `:302` | constants + path helpers **live** (used by the 44 save call sites); never read back |
| Session bootstrap always fresh | `session.rs:280` fresh `ctx`, `:302` `UiTranscript::new()`, `:319` `subagents = Vec::new()` |

Candidate seams (narrowest first):
1. **`src/ui/session.rs:302`** — replace `UiTranscript::new()` with `if resume { UiTranscript::load(&ui_transcript_path)? }`; plus `renderer.rehydrate_*` equivalent, and `steering_history` re-fill (mirror `31f4e90:session.rs:81-102`). Insert **after** `:300-301` (paths known) and **before** `:319`.
2. **`src/ui/session.rs:280`** — after `build_manager_context(cfg, system.clone())`, load the manager transcript and re-pin the system prompt (the prior art did exactly `set_system_prompt` after load, `31f4e90:session.rs:70-77`). Note `ContextEngine.messages` is private (`context.rs:558`) → must go through `load_transcript`.
3. **`src/ui/session.rs:319`** — `let mut subagents = …` becomes the worker-restore point (`Vec<SubagentDetail>` from a new per-worker journal; `set_subagents` at `:427`/`renderer.set_subagents`).
4. **`src/main.rs:56-58`** — orchestrator-level restore *after* `boot_manager` and *before* `rt.block_on` (needs the runtime; `OrchestratorManager` currently has no journal field — the prior art added one at `31f4e90:orchestrator/mod.rs:102/139`).
5. **`src/main.rs:16` + `:210` + `:213-220` + `:59-61`** — the flag itself (see 1.1.1); or add a `Config` key next to the deleted `enable_rehydration` (`31f4e90:src/config.rs:128-129,155,199,261,342`).
6. **`src/ui/session.rs:391`** (`while !renderer.aborted()`) — a resume run must arm the loop without re-asking for a goal: today the goal comes from `initial` (`:341-345`) or `read_input` (`:346-377`), and `:381-382` re-appends a `UiRecord::User` (would duplicate the restored goal).

---

## 4. Atomic write + thread-safety constraints

**Existing atomic helper (reuse this, do not hand-roll):** `crate::harness::fs::write_file_atomic(path, bytes)` — `src/harness/fs.rs:475-503`: stages via `create_staging_file` (`:384-400`, name `.{stem}.tmp.{pid}.{nanos}.{seq:08}`, `create_new`/`O_EXCL`, `STAGING_SEQ` `:371`), preserves target permissions, `std::fs::rename` **in the same directory** (`:493`), and `StagedTemp::drop` removes the staged file on every failure path (`:454-460`). Per-path sharded serialisation for concurrent edits: `REPLACE_LOCKS: [Mutex<()>; N]` `:334`. Design note `:17-25`. Only other renames: log rotation `src/harness/workspace.rs:143-146`.

**Everything persistence-relevant today is a plain, non-atomic `fs::write`:** `manager/context.rs:993` (`save_transcript`, pretty JSON of `Vec<Message>`), `ui/transcript.rs:99` (`UiTranscript::save`), `manager/phase.rs:327/558/560/610/640/642` (plan + archives), `agents/prompt_builder.rs:131/683`. Consequence: kill-during-write leaves a truncated file, and both loaders fail open (see §2.4-5). Ordering hazard worth noting: `session.rs:689-690` writes the assistant `tool_calls` message **before** dispatch, so a crash between that write and the round's tool results leaves a half-paired transcript — the in-process repair (`ensure_tool_call_pairs`, `manager/context.rs:897-903`; boundary call `session.rs:1788-1794`; `ABORTED_TOOL_RESULT = "(aborted)"` `context.rs:327`) never runs on a crash, so a resume must re-run the repair itself (it is idempotent).

**Thread-safety / capturability constraints on the structures you would snapshot:**
- `Renderer` is `pub trait Renderer: Send` (`src/ui/mod.rs:88`); `TuiRenderer` holds `Cell`s and `RefCell`s (`src/ui/tui/mod.rs:100-122`: `cached_message_lines`, `cached_rendered_messages`, …) → `Send` but **not `Sync`**. A UI-model snapshot must therefore be taken **on the session thread** from `&mut TuiRenderer` inside `run_session`, or via a new `Renderer::snapshot()` trait method — it cannot be pulled from another thread or from the panic hook on a different thread.
- `ContextEngine` is `Send` (`messages: Vec<Message>` + `Option<Arc<HarnessStats>>`, `context.rs:555-571`) with **private** `messages` → snapshot/restore only through the existing `save_transcript`/`load_transcript` pair, or a new `pub fn snapshot()`.
- Worker registry is a `LazyLock<DashMap<String, WorkerState>>` (`workers.rs:106`): sharded locks, and the codebase's own rule is "never hold a DashMap iterator while mutating" (`workers.rs:196-207`, `notice.rs:690`). `WorkerState`/`ActiveWorkerInfo` are **not serde-serializable** (`std::time::Instant`, `CancellationToken`) → a snapshot needs a projection type + `started_wall` (`chrono`) as the durable clock.
- `OrchestratorManager` fields are all `Send + Sync` (`orchestrator/mod.rs:88-114`, incl. `delegation_events: Arc<Mutex<Vec<DelegationEvent>>>`); `Specialist: Send + Sync + Debug` (`agents/mod.rs:181`) but its conversation lives in the async fn (`execution.rs:175`+, engine local) → **no `&mut` handle exists anywhere** to snapshot it.
- Poison-tolerance precedent (relevant to the panic hook): every `PLAN_MUTEX` site uses `lock().unwrap_or_else(|e| e.into_inner())` (`manager/phase.rs:317/361/382/441/455/509/538/595`); `CrashJournal` used the same pattern (`31f4e90:freeze.rs:106/130/139/151/180/196`). A snapshot from the panic hook is therefore feasible **if** it (a) uses `into_inner` on poisoned locks, (b) never panics (see the explicit reasoning in `workers.rs:196-207`: a panic inside `Drop` during unwind aborts the process), (c) uses only `LazyLock`/`Mutex`/`RwLock` statics — the hook runs **outside** the async runtime, so `block_on` there is unsafe-ish (`block_on_safe`, `harness/mod.rs:291-312`, would `panic!` if the runtime can't be built) → the hook path must be pure sync I/O.
- Lock-ordering: `WORKERS` (DashMap) and `notice::WORKER_INBOXES` (DashMap) are separate maps and `ActiveWorkerGuard::drop` deliberately takes no guard (`workers.rs:186-207`) — any snapshot walker must collect keys into a `Vec` first, exactly like the completed-ring loop at `:171-190`.

---

## 5. Open risks for exact restore (max 10)

1. **No hook exists for SIGTERM/SIGHUP/SIGINT and the panic hook has no persistence slot** — the two paths that actually describe a crash (`main.rs:276-282`, no handler at all otherwise). Any "exact" story must add a sync-only snapshot into `main.rs:277` and a real signal handler; `main.rs:64-65` (300 ms `shutdown_timeout`) is too late and too short for a cooperative flush of every worker.
2. **Worker conversation state is unreachable**: the specialist `engine` is a local in `run_specialist_live_inner` (`execution.rs:175`+), `IsolatedContext` is the *seed* only (`agents/mod.rs:129-137`), and no `save_transcript` exists anywhere in `src/agents/` — subagent history requires a new journaling point inside the worker loop (fix-loop state in `agents/runner/fix_loop.rs` likewise).
3. **`spawn_blocking` + `thread::scope`/`block_on` layering** (`session.rs:860/884/913/934`, `delegate.rs:101-143`, `harness/mod.rs:291-312`) means `JoinHandle::abort()` and `rt.shutdown_timeout` cannot interrupt a worker: teardown is cooperative-only, so "the same in-flight subagents" can be mid-tool-call (PTY, file write) with no consistent boundary.
4. **Crash leaves half-paired transcripts** (assistant `tool_calls` persisted at `session.rs:689-690` before dispatch; the repair at `:1788-1794` is in-process only) and every loader **fails open** (`context.rs:1003-1015`, `transcript.rs:76-85`) → a partially written file silently becomes a brand-new session.
5. **Non-atomic writes** (`context.rs:993`, `transcript.rs:99`) while `harness::fs::write_file_atomic` (`:475`) already solves it — ordering/atomicity of the two files is also unsolved: ctx and UI transcript are written at different points (44 sites) and can disagree after a crash.
6. **UI pane text is not fully journable today**: `UiRecord` has no variant for streamed subagent `SubagentMessage`/`SubagentThinking` (`transcript.rs:14-40`), partial streamed `current_content`/`current_thought` (`tui/mod.rs:441-470`) is uncommitted until the turn ends, and no pane geometry/scroll/draft/focus state (`tui/mod.rs:40-122`) is serializable. Byte-exact "same text in every pane" therefore needs a new record vocabulary **plus** a renderer-state snapshot — the old `rehydrate_ui` synthesized prefixes and was not byte-exact (`31f4e90:tui/mod.rs:1086+` vs live `tui/mod.rs:585/618/765`).
7. **Volatile steering/notice state**: `PENDING_NOTICES` / `WORKER_INBOXES` / `REPLIED_NOTICES` (`notice.rs:76/84`), `steering_history` (`bus.rs:13`), `delegation_events` (`orchestrator/mod.rs:111`) are in-memory queues — a restart loses notices that were queued but not delivered, changing the manager's next request.
8. **Identity drift**: worker correlation keys are minted at registration (`{agent}-{task}` / `{agent}-wN` with `WORKER_ID_SEQ`, `workers.rs:229-260`) and the completed ring is capped at 10 (`:100`), while prior-art `worker_id` was a fresh uuid per snapshot (`freeze.rs:105-111`) — exact restore needs stable ids carried in the snapshot, otherwise restored workers re-register under different keys.
9. **Destructive paths in the normal loop**: `/reset` deletes the plan **and both transcripts** (`helpers.rs:197-218`, `phase.rs:407-421`), and plan archiving deletes them on completion (`phase.rs:561-568`); also the steering-abort branch (`session.rs:1900-1923`) resets cancellation and rewrites subagent state without exiting — a resume feature must not mistake these for crash checkpoints, and `--resume` needs a "session is finished/cleared" verdict distinct from "session was interrupted".
10. **Orphaned OS children survive the restart**: PTY groups are only SIGKILLed cooperatively (`pty.rs:262-266`, `:1560-1570`) and there is no `Pdeathsig` — after a hard kill, tool subprocesses may still be mutating the workspace while the "restored" session re-applies the same edits (double-apply / file races). Restoring "exact" state must decide: re-adopt, refuse, or kill-and-replay.
