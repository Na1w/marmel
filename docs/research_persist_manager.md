# Manager context persistence surface (session restore research)

Scope: the MANAGER context only. Read-only research, no source modified.
Crate `marmennill`, branch `1.0.0-refactor`, HEAD `a5e6763`.
Note: the type the task calls `MessageManager` is named **`ContextEngine`** in this tree
(`src/manager/context.rs:555`, re-exported as `manager::ContextEngine`, `src/manager/mod.rs:18`).

## State fields

`pub struct ContextEngine` — `src/manager/context.rs:554-571` (`#[derive(Debug, Clone)]`, **no serde derive**):

| Field | Line | Kind | Persisted by `save_transcript`? |
|---|---|---|---|
| `messages: Vec<Message>` | `:558` | **conversation state** — the only transcript container. `[0]` = system prompt, `[1]` = pinned goal (doc `:556-557`) | **yes** (it is the whole file) |
| `max_context_tokens: usize` | `:560` | budget | no (rebuilt from `cfg.max_context_tokens`, `src/ui/session.rs:183`) |
| `stats: Option<Arc<HarnessStats>>` | `:562` | shared intervention counters | no (and never wired for the Manager engine: `set_stats` at `:587` has **no live caller**, only `context_tests.rs:256`) |
| `rebirth_advisory_emitted: bool` | `:564` | flag (80 % advisory latch) | no |
| `consecutive_rebirths: usize` | `:566` | counter (rebirth cooldown gate, `should_advise_rebirth` `:691-696`) | no |
| `tool_schema_tokens: usize` | `:570` | cached BPE cost of advertised tool schemas | no (re-charged by `sync_manager_tool_schema`, `src/ui/session.rs:198-215`) |

Constructor defaults: `:574-583`. Everything except `messages` is therefore **derived or reset** on restart.
Session-loop state that is *not* in the engine at all (all locals in `run_session_with_bounds_report`):
`goal: String` (`:342-378`), `turn_count`/`nudge_count` (`:468-469`, per-request, cap `MAX_TURNS = 100`,
`src/manager/loop.rs:51`), `steer_queue` (`:317`), `subagents` (`:318`), `watchdog`/`failures` (`:327-328`),
`session_stops` (`:387`), `steering_history` (`:296`), `UiTranscript` (`:302`).

## save/load exact behaviour

```rust
// src/manager/context.rs:987-995
/// Save the full message transcript to disk as JSON.
pub fn save_transcript(&self, path: &std::path::Path) -> anyhow::Result<()> {
    if let Some(parent) = path.parent() { std::fs::create_dir_all(parent)?; }
    let json = serde_json::to_string_pretty(&self.messages)?;   // ONLY `messages`
    std::fs::write(path, json)?;                                // non-atomic overwrite
    Ok(())
}
// src/manager/context.rs:996-1018
/// Load the message transcript from disk if it exists and contains at least 2 messages (system + goal).
pub fn load_transcript(&mut self, path: &std::path::Path) -> anyhow::Result<bool> {
    if !path.exists() { return Ok(false); }
    let data = std::fs::read_to_string(path)?;
    let msgs: Vec<Message> = match serde_json::from_str(&data) {
        Ok(m) => m,
        Err(e) => { tracing::warn!("Failed to deserialize session transcript from {}: {e}", path.display()); return Ok(false); }
    };
    if msgs.len() < 2 { return Ok(false); }
    self.messages = msgs;   // wholesale replace; no repair, no role check, no merge
    Ok(true)
}
```

Serialized shape = a **bare JSON array** of `Message` (not an object): `serde_json::to_string_pretty(&self.messages)`
(`:992`), using the wire serde attrs of `Message` (`src/types/wire.rs:7-28`): internally tagged `"role"`
(`system` / `user` / `assistant` / `tool`). Verified against the live file `.marmel/.session_transcript.json`
(18 messages, keys `role`, `content`, `reasoning_content`, `tool_calls`, `tool_call_id`).

Non-test callers / call sites:
- **`load_transcript`: zero callers anywhere.** `grep load_transcript` over the whole workspace returns only
  its definition (`src/manager/context.rs:998`) and a doc mention (`:268`) — nothing in `src/`, `src/manager/context_tests.rs`,
  or `tests/`. Rehydration call sites were removed (git `31f4e90` "disable session rehydration by default", then
  deleted in the refactor); there is no `enable_rehydration` key in `src/config.rs` or `marmel.toml.example`, and
  no `rehydrate*` function survives in `src/ui/` (`grep rehydrate`: no hits outside docs).
  `UiTranscript::load` (`src/ui/transcript.rs:76`) is likewise only referenced by its own test (`:335`);
  the live session starts from `UiTranscript::new()` (`src/ui/session.rs:302`).
- **`save_transcript`: 22 sites in `src/ui/session.rs`** (all `let _ = …`, i.e. write errors silently ignored):
  `690` (assistant message, incl. `tool_calls`, appended), `717` (after `compact()`), `727` (after rebirth advisory),
  `956` (renderer-flush hard error, after `ensure_tool_call_pairs`), `1224` (parallel tool result), `1272`
  (join-loop flush error, after repair), `1371` (rebirth checkpoint applied), `1407` (rebirth continuation notice),
  `1413`, `1422`, `1441`, `1460`, `1485` (per-tool ERROR results), `1734` (sequential tool result), `1810`
  (after `compact()`), `1820` (advisory), `1848` (plan-complete notice), `1884` (bound-stop notice), `1916`
  (steer drain), `1932` (steer drain + continue), `1958` (new user line), plus `2166` inside
  `ensure_pairing_at_boundary` (`:2153-2176`, the only site that saves *after* a pairing repair).
  Ctrl-C is an in-loop abort (`renderer.request_abort()`, e.g. `src/ui/bridge/sink.rs:268,278`;
  `src/ui/tui/mod.rs:920`) — it reaches the boundary repair at `:1788-1796`; there is **no SIGINT/`tokio::signal`
  handler in the crate** (`grep SIGINT|tokio::signal|ctrlc`: no hits), so a hard kill/crash only leaves the last
  intermediate write (e.g. `:690`, which is *unrepaired*).

## What is lost on reload

- Everything except `messages`: `max_context_tokens`, `stats` counters (`HarnessStats`, `src/harness/common.rs:35-50`:
  `repetition_breaks`, `empty_prods`, `context_compactions`, `xml_tool_rescues`, `backend_retries`,
  `session_rebirths`, `steer_arbitrations`), `rebirth_advisory_emitted`, `consecutive_rebirths`, `tool_schema_tokens`.
- All session-loop/turn state: `turn_count`, `nudge_count`, watchdog/failure budgets, `session_stops`,
  in-flight `subagents`, `steer_queue`, `steering_history` (`src/ui/session.rs:296,317-328,387`).
- **UI pane text** — it never enters the manager transcript; it lives in `UiTranscript`
  (`.marmel/.ui_transcript.json`, `src/ui/transcript.rs:44-49`, `TRANSCRIPT`/`UI_TRANSCRIPT_FILE` in
  `src/manager/phase.rs:117,119`). Two independent journals, no cross-referenced ids, no shared sequence number.
- **Lossy round-trip inside `messages` itself:** `Assistant.content: Option<String>` is written through
  `serialize_assistant_content` (`src/types/wire.rs:16,30-41`), which serializes `None` as `""`; on load `""`
  deserializes to `Some("")`, not `None`. So `None → "" → Some("")` — not byte-identical to the pre-crash value.
  `reasoning_content` (`:18-19`) and `tool_calls` (`:20-21`) round-trip fine (`skip_serializing_if` + `default`),
  i.e. empty `tool_calls` is omitted and re-read as `vec![]`.
- File-level hazards: `std::fs::write` is non-atomic (`:993`) and every caller swallows the error, so a crash
  mid-write yields a corrupt file that `load_transcript` turns into `Ok(false)` + one `tracing::warn!` (`:1005-1011`)
  — i.e. **silent total loss**, no partial recovery, no backup file, no version/schema tag, no session id,
  no timestamps.

## Pinned-message invariants

Established once, in this order (live Manager path):
1. `build_manager_context` → `ctx.set_system_prompt(system_prompt)` (`src/ui/session.rs:184`, built at `:277`).
2. After the first goal is known: `ctx.set_goal(goal.clone())` (`src/ui/session.rs:380`).

```rust
// src/manager/context.rs:591-598  (REQ-CORE-001)
pub fn set_system_prompt(&mut self, prompt: String) {
    if self.messages.is_empty() { self.messages.push(Message::System { content: prompt }); }
    else { self.messages[0] = Message::System { content: prompt }; }
}
// src/manager/context.rs:600-612  (REQ-CORE-002)
pub fn set_goal(&mut self, goal: String) {
    if self.messages.is_empty() { self.messages.push(Message::System { content: String::new() }); }
    if self.messages.len() == 1 { self.messages.push(Message::User { content: goal }); }
    else { self.messages[1] = Message::User { content: goal }; }
}
```

Enforcement is by *index convention*, not validation: nothing anywhere asserts `messages[0].role == "system"`
or `messages[1].role == "user"`. The compaction side:

```rust
// src/manager/context.rs:270-285
fn pinned_prefix_end(messages: &[Message]) -> usize {
    messages.iter().enumerate()
        .rposition(|(i, m)| i >= 2 && match m {
            Message::System { content } | Message::User { content } =>
                content.starts_with(REBIRTH_CHECKPOINT_PREFIX),
            Message::Assistant { .. } | Message::Tool { .. } => false,
        })
        .map(|idx| idx + 1).unwrap_or(2)     // default pinned window = [0],[1]
}
```
`compact()` keeps `messages[..prefix_end]` unconditionally (`:754-760`) and reports
`CompactionOutcome::TargetUnreachable` if the pinned prefix alone exceeds the 70 % target (`:762-786`).
`perform_rebirth` re-reads the pins positionally (`:930-941`): `messages.first()` must be `Message::System`
and `messages.get(1)` must be `Message::User`, **else it silently substitutes `String::new()`** and collapses
the transcript to a system-less/goal-less checkpoint shape.

Naive `load_transcript` reload (its exact current semantics) would:
- **Wholesale-replace `self.messages`** (`:1016`) — no role/shape check beyond `msgs.len() >= 2` (`:1013`),
  so a disk file whose `[0]`/`[1]` are not System/User is accepted.
- Then be **overwritten by the live sequence**: `set_system_prompt` at boot (`session.rs:184`) writes index 0,
  and `set_goal` at `:380` writes index 1 — with the *new* CLI arg / newly typed input. A restored goal is
  therefore clobbered unless the caller supplies the identical string; a restored *system prompt* is replaced by
  the freshly rendered prompt (`load_system_prompt_with_plan_and_warning`, `session.rs:274`), which changes with
  plan state/config → KV-cache prefix break (REQ-CORE-001) on every restore.
- Order matters: if `load_transcript` runs *after* `build_manager_context` (System at `[0]`, len 1) the load wipes
  that `[0]`; if it runs before, the boot `set_system_prompt`/`set_goal` rewrite `[0]`/`[1]`.
- `pinned_prefix_end` already anticipates historic drift: transcripts on disk may carry the rebirth checkpoint as
  the **`System`** variant while `perform_rebirth` emits it as **`User`** (bug M12 note, `:266-269`), which is why
  matching is content-based. `INJECTED_ADVISORY_PREFIXES` (`:318-323`) names a `SYSTEM: CONTEXT LIMIT EXCEEDED`
  marker that only exists in pre-deletion transcripts.

## Message type & serde status

`src/types/wire.rs:7-28` — already `#[derive(Debug, Clone, Serialize, Deserialize)]`, `#[serde(tag = "role", rename_all = "lowercase")]`:
- `Message::System { content: String }` → `"system"`
- `Message::User { content: String }` → `"user"`
- `Message::Assistant { content: Option<String>, reasoning_content: Option<String>, tool_calls: Vec<ToolCall> }` → `"assistant"`
- `Message::Tool { tool_call_id: String, content: String }` → `"tool"` (no `name` field)

`ToolCall` (`:73-80`): `{ id: String, kind: String (serde key `type`, default `"function"`), function: ToolFunction }`;
`ToolFunction` (`:155-159`): `{ name: String, arguments: String }` — `arguments` is a **JSON string**, sanitized by
`ensure_valid_json_arguments` (`:85-105`; malformed input becomes `{"_error":"malformed_arguments","_raw":…}`,
detectable via `ToolCall::is_malformed` `:140-151`). `Message` has accessors `role()/content()/reasoning_content()`
(`:42-69`) and **no `PartialEq`** (`src/manager/context_tests.rs:1071`). No transcript envelope type exists —
the file format is exactly `Vec<Message>` JSON, so any added metadata (session id, version, pin witnesses,
subagent linkage) requires a new format and a migration path.

## Repair/normalization today

**On load: none.** `load_transcript` (`:998-1018`) assigns verbatim.

The repair primitives exist but run only on the *live* path:
- `prune_orphan_tool_messages(Vec<Message>) -> Vec<Message>` — `:338-357`: drops `Message::Tool` whose
  `tool_call_id` has no surviving assistant parent (one half only).
- `repair_tool_call_pairs(Vec<Message>) -> Vec<Message>` — `:384-433`: (1) drop orphan/duplicate tool results,
  (2) re-emit each result directly under its parent assistant and **synthesize `Message::Tool { content: ABORTED_TOOL_RESULT }`**
  (`ABORTED_TOOL_RESULT = "(aborted)"`, `:331`) for every `tool_calls` entry with no result. Never reorders
  `System`/`User`, so pins keep their indices (`:373-376`).
- `ContextEngine::ensure_tool_call_pairs() -> bool` — `:897-903` (idempotent, `pairing_shape` diff `:439-455`);
  live callers: `src/ui/session.rs:505` ("before turn-budget exit"), `:582` ("before request"), `:1788`
  ("after tool round") via `ensure_pairing_at_boundary` (`:2153-2176`, repairs then saves at `:2166`), plus
  `:955` and `:1271` on renderer-flush error paths. `compact()` also calls the pass (`:784`, `:800-822`).
- The dangling hazard is documented live: `docs/recon_bugs_manager.md:31`. Empirically the **current on-disk
  transcript is already unpaired**: 13 assistant `tool_calls` ids vs 9 tool messages → 4 dangling calls
  (0 orphans) — proof that most `save_transcript` sites (`:690`, `:1224`, `:1734`, …) write before any repair.

## Implications for exact restore

- **`load_transcript` is the right hook but must not be reused as-is**: it is uncalled, unpaired, unchecked,
  and wholesale-assigning. Exact restore needs (a) role assertions on `[0]`/`[1]` after load, (b)
  `ensure_tool_call_pairs()` immediately after load (or refusal to start), (c) skipping `set_goal` when a
  transcript is restored (`session.rs:380`), and (d) pinning the *stored* system prompt instead of re-rendering it
  (`session.rs:277,284`), otherwise REQ-CORE-001/002 are violated on every restart.
- **The system prompt is a moving target**: it is composed from `cfg` + plan state at boot
  (`load_system_prompt_with_plan_and_warning`, `session.rs:274`), so exact restore requires persisting the exact
  prompt text (or a byte/hash witness) rather than re-deriving it. Same for the tool schema charge
  (`tool_schema_tokens`, `:570`), which is re-priced from live MCP state (`session.rs:198-215`) — if the MCP set
  changed across restart, the *budget* differs although the transcript is identical.
- **A bare `Vec<Message>` envelope is insufficient** for the stated goal: no version, no session id, no pin
  witnesses, no counter values, no linkage between manager messages and subagent records. Add a versioned object
  (`{ version, session_id, saved_at, engine: {…fields…}, messages: [...] }`) with a reader that still accepts the
  legacy bare-array shape (`.marmel/.session_transcript.json` exists today in that shape, 109 KB live).
- **Non-transcript engine state must be persisted or intentionally reset**: `rebirth_advisory_emitted` and
  `consecutive_rebirths` change behaviour after restart (advisory may re-fire, `:691-696`); `HarnessStats` counters
  reset (and are not attached to the Manager engine at all — `set_stats` has no live caller).
- **Save points are lossy by construction**: 21 of the 22 call sites ignore errors and only one
  (`:2166`) guarantees a paired write. A crash between `:690` (assistant with `tool_calls`) and the round's tool
  results reproduces exactly the dangling state that today only `ensure_pairing_at_boundary` heals.
- **Deletion paths must be respected**: `Plan::clear` (`src/manager/phase.rs:407-421`) and `Plan::archive`
  (`:562-569`) **delete both transcript files**, and `/reset` deletes them (`src/ui/helpers.rs:203-206`,
  `src/ui/session.rs:359-363,437,1943-1951`). A restore feature must treat these as intentional "do not restore"
  signals, not as missing files.
- **Path is CWD-relative**: `TRANSCRIPT_FILE = ".session_transcript.json"` (`src/manager/phase.rs:117`), joined onto
  `Plan.dir` (`Plan::transcript_path` `:298-300`); `Plan.dir` defaults to `MARMEL_DIR = ".marmel"`
  (`:106`, `Plan::default` `:269-273`), and `run_session` uses `manager.plan.clone().unwrap_or_default()`
  (`src/ui/session.rs:270`) with the Manager booted from `Plan::default()` (`src/main.rs:188`). Nothing
  canonicalizes or roots the path (`Workspace::at`/`new`, `src/harness/workspace.rs:37-58`, also `.marmel`), so
  restart from a different working directory silently finds no transcript (`Ok(false)`, `:999-1000`).
- **Two journals must be joined by an explicit ordering key**: `.session_transcript.json` (manager LLM view) and
  `.ui_transcript.json` (pane text, `src/ui/transcript.rs:44-49`) are written independently at different points
  (`ctx.save_transcript` vs `ui_transcript.save`), so an exact pane restore cannot be derived from the manager
  transcript, and the "exact set of in-flight subagents" is not in either file today (`subagents: Vec<…>`,
  `src/ui/session.rs:318`, memory-only).
