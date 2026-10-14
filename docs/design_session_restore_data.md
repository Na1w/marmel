# Session rehydration — Design A: the durable data model (§1–§5)

| | |
|---|---|
| **File** | `docs/design_session_restore_data.md` |
| **Task** | t-018 (sections 1–5). Sections 6–10 are appended by **t-019** to this same file. |
| **Status** | **Normative.** The contract below was fixed by the Manager (`Execution Plan → "Design A contract"`); this document *expands* it, it does not re-litigate it. |
| **Verified against** | branch `1.0.0-refactor`, HEAD `bbfb893`. Every `path:line` in §2 was opened and read in this working tree. |
| **Inputs** | `docs/research_persist_manager.md`, `docs/research_persist_ui.md`, `docs/research_restore_lifecycle.md`, `docs/cleanup_report.md` §2–§3, and source: `src/types/wire.rs`, `src/manager/context.rs`, `src/manager/phase.rs`, `src/ui/transcript.rs`, `src/ui/mod.rs`, `src/orchestrator/workers.rs`, `src/harness/fs.rs`, `src/agents/runner/execution.rs`. |
| **Goal (definition of done for the feature)** | After Ctrl-C / `kill` / crash + restart, the session resumes with **the same in-flight subagents**, **each subagent's full message history**, **the manager's exact message history**, and **the exact text in every UI pane**. |

**Language:** MUST / SHALL = violation is a defect (and, per D6, a `RestoreOutcome::Refused`). SHOULD = deviation needs a written reason in the PR that deviates. MAY = optional.

**Out of scope for §1–§5 (owned by t-019):** the write/commit protocol and its ordering guarantees (§6), the FNV-1a fingerprint function and corruption-detection decision tree (§7), on-load normalization rules and the full `RepairReport` semantics (§8), exactness acceptance criteria (§9), the non-restorable inventory (§10). Where §1–§5 need one of those, it is forward-referenced, never invented.

---

## 1. Normative decisions

### D1 — `src/session/` is the single owner of the durable session format

- The crate SHALL contain a new module `src/session/` and it SHALL be the **only** place in the crate that (a) names a session file, (b) declares a `Serialize`/`Deserialize` session type, (c) computes a bundle fingerprint, (d) performs the atomic commit, (e) loads/repairs/refuses a bundle, or (f) converts a live runtime value into a durable projection.
- No other module SHALL define, hand-roll, or "temporarily" duplicate a session format, a session file path constant, or a session (de)serialization routine. A test fixture MAY build JSON literals, but never through a second production serializer.
- `src/session/` SHALL expose, and be the sole owner of, these seams (module split is prescriptive so later tasks land in disjoint files):
  - `session::format` — constants (`FORMAT_VERSION`, `SCHEMA_VERSION`, file names) + every §3 type.
  - `session::time` — the `Instant` ↔ wall-clock projections of D5 (sole owner of `Instant` conversion for sessions).
  - `session::commit` — write of members + manifest-last commit (`D3`), using `harness::fs::write_file_atomic`.
  - `session::restore` — read, verify, refuse, repair, and the `RestoreOutcome` decision.
  - `session::worker_journal` — the per-worker mirror of §5.
- **Consequence for callers:** `ContextEngine`, `UiTranscript`, `TuiRenderer`, and the worker registry each gain at most one narrow seam (a snapshot getter and/or a restore installer) typed with `session::format::*`. They SHALL NOT own the format.

### D2 — Both existing journals are superseded and deleted

- SHALL delete `ContextEngine::save_transcript` (`src/manager/context.rs:988`) and `ContextEngine::load_transcript` (`src/manager/context.rs:998`), and the constant `TRANSCRIPT_FILE` (`src/manager/phase.rs:117`).
- SHALL delete `UiTranscript::save` (`src/ui/transcript.rs:89`) and `UiTranscript::load` (`src/ui/transcript.rs:76`), and the constant `UI_TRANSCRIPT_FILE` (`src/manager/phase.rs:119`).
- `UiTranscript` SHALL remain as an **in-memory** journal (append-order for steering/live bookkeeping) and SHALL keep `extract_steering_history` (`src/ui/transcript.rs:113`) with its current semantics. It MUST NOT become a persistence type again, and it MUST NOT be re-purposed as the UI pane authority (that is D4).
- SHALL repoint `Plan::transcript_path()` (`src/manager/phase.rs:298`) and `Plan::ui_transcript_path()` (`src/manager/phase.rs:302`) to one new accessor `Plan::session_bundle_path() -> PathBuf` returning `.marmel/session` (a **directory**; see D3). The two old accessors SHALL be deleted, not aliased, so that every remaining caller is forced to decide what it actually wants.
- SHALL repoint the two destructive lifecycle sites — `Plan::clear` (`src/manager/phase.rs:407-421`) and plan archive (`src/manager/phase.rs:562-569`) — from "delete two loose files" to "delete/recycle the one bundle directory". Both sites keep their current meaning: **an explicit "do not restore" signal** (see D6 / `ProcessEnding::Reset`, `ProcessEnding::PlanArchived`).
- The 22 `ctx.save_transcript(..)` and 22 `ui_transcript.save(..)` call sites (all in `src/ui/session.rs`, all `let _ = …`) SHALL be replaced by session-owned commit triggers. A call site SHALL NOT ignore a commit failure the way `let _ =` ignores it today: the commit result feeds the capture-trigger bookkeeping designed in t-006.
- **Migration:** the legacy files `.marmel/.session_transcript.json` and `.marmel/.ui_transcript.json` (bare `Vec<Message>` array / `UiTranscript` object, no version, no session id) SHALL NOT be silently upgraded into a bundle. A `--resume` against a workspace that has only legacy files SHALL report `RestoreOutcome::NoBundle` plus a dedicated "legacy journals found, not restorable" warning naming both paths. `ProcessEnding` is unavailable for them, so the fail-closed rule (D6) forbids guessing.

### D3 — Split-file bundle, manifest-last commit is the atomicity point

- The bundle SHALL be exactly this layout under `Plan::session_bundle_path()`:

  ```text
  .marmel/session/
    manifest.json          <- committed LAST; the atomicity point
    manager.json           <- Manager ContextEngine section
    ui.json                <- UI pane-text section
    workers/<key>.json     <- one file per worker (key rule in §5.2)
  ```
- Every member write (including `manifest.json`) SHALL go through `harness::fs::write_file_atomic` (`src/harness/fs.rs:475-503`) — stage to a unique `O_EXCL` name in the same directory, preserve target permissions, `rename` in the same directory, unlink the staged file on every failure path. A session writer SHALL NOT call `std::fs::write` for a bundle member (that is precisely how today's journals get truncated: `context.rs:993`, `transcript.rs:96-97`).
- The commit order SHALL be: all member files → **then** `manifest.json`, and nothing after the manifest. Therefore a `manifest.json` that parses and whose `members[]` all verify describes a set of member files that were each fully written before it (they are atomic renames, so a reader sees old-or-new, never mixed).
- `manifest.json` SHALL be the only file that names the member set. A member file that exists but is not listed in `members[]` is an orphan from an interrupted overwrite and SHALL be ignored (never merged).
- No cross-file consistency MAY be assumed without the manifest: manager and UI state are written at different points in today's loop (44 sites) and can disagree after a crash (`docs/research_restore_lifecycle.md` §5-5). The manifest's per-member `saved_wall` / `saved_elapsed_secs` / `fingerprint` is what makes such a disagreement *detectable* rather than silent (§7, t-019).

### D4 — Pane text strings are the UI byte authority

- The UI section's authority for "the exact text in every pane" SHALL be a list of **rendered strings**, not a replayable event log: `UiSection.chat_messages: Vec<String>` (D1 type, §3.6), mirroring the live pane buffer `TuiRenderer.messages: Vec<String>` (`src/ui/tui/mod.rs:43`).
- Restore SHALL assign those strings to the pane buffer. A replay-to-rebuild path (re-rendering `UiRecord`s into pane lines, as the deleted prior art did: `31f4e90:src/ui/tui/mod.rs:1086+` synthesized `"User: {text}"` / `"[Tool Call] {display}"` while the live path spelled them at `src/ui/tui/mod.rs:585`, `:618`, `:765-769`) is **prohibited**: byte equality achieved by two independent formatters is not equality.
- `UiRecord` (§2.3, `src/ui/transcript.rs:11-40`) SHALL NOT be extended into the pane authority. It stays what it is today — an 8-variant semantic journal — and it is not sufficient for the goal: it has **no variant for subagent pane text** (`SubagentMessage`/`SubagentThinking`, `docs/research_persist_ui.md` "Complete UI state inventory") and it stores bare `display`/`text` without the pane prefixes the screen shows.
- Therefore the bundle carries *both*: the pane strings (authority, D4) and — where needed for steering-history restoration — the in-memory `UiTranscript` rebuilt from its own section. When the two disagree, `chat_messages` wins for display, and `extract_steering_history` (`src/ui/transcript.rs:113`) wins for steering correlation. Neither is derived from the other at load time.

### D5 — Non-serializable state becomes an explicit projection, never a fake

- `std::time::Instant` SHALL NOT be serialized. Its durable form is a pair: an `f64` number of seconds **offset from a process-monotonic anchor** (`Manifest.anchor_wall` fixes that anchor's wall-clock position) **plus** an absolute wall-clock reading. Rule text and formulas: §5.6; owning module: `session::time`. The accepted precedent for re-anchoring an `Instant` from wall-clock data is `Plan::get_plan_start_time()` (`src/manager/phase.rs:223-235`), which re-derives `Instant::now() - elapsed` from a file's mtime — same shape, same documented approximation.
- `CancellationToken`, `tokio::task::JoinHandle`, PTY handles/groups, and RAII guards (`ActiveWorkerGuard`, `workers.rs:186-207`) SHALL NOT be serialized, and SHALL NOT be "reconstructed" by minting fresh equivalents behind the caller's back. Each SHALL be replaced by (a) a **status enum** capturing what the state *means*, and (b) a `NonRestorableNotice` recording that the capability is gone and what the user should expect.
- `NonRestorableNotice` (declared in §3.11) is a first-class, user-visible output of a restore, not a log line. Every projection that lost capability SHALL emit one. A restore that silently re-mints a cancellation token, re-attaches a dead PTY, or fabricates a `JoinHandle` is a defect.
- `WorkerState` / `ActiveWorkerInfo` / `CompletedWorkerInfo` (`src/orchestrator/workers.rs:26-77`) are `#[derive(Debug, Clone)]` with **no serde** and hold `Instant` + `Option<CancellationToken>`; `SubagentDetail` (`src/ui/mod.rs:19-37`) likewise holds `Instant`. All three SHALL be projected by `session::format`, never annotated into serde shape.

### D6 — Fail-CLOSED

- An **unclean envelope** SHALL be refused: JSON that does not parse, a `format_version`/`schema_version` this binary does not implement, a `deny_unknown_fields` violation, a manifest member that is missing / wrong-length / fingerprint-mismatched, or a section that fails its structural preconditions ⇒ `RestoreOutcome::Refused`.
- A **pin violation** (REQ-CORE-001/002, §4) SHALL be refused.
- **Unstable normalization** SHALL be refused: after running the on-load normalization passes (§8, t-019), a second pass must be a no-op (the passes are documented idempotent, `src/manager/context.rs:887-890`); if it is not a no-op, the loader SHALL refuse rather than ship an ambiguous transcript.
- On `Refused`, the bundle bytes SHALL be left untouched: no rewrite, no "helpful" repair in place, no deletion, no rename-to-`.bak`. `session::restore` SHALL be read-only with respect to disk. The session SHALL then start fresh (or abort, per t-006's decision) with the refusal reported **loudly** — the refusal reason, the offending member path, and the fingerprint/length mismatch numbers.
- The **only tolerated partial** SHALL be: exactly one corrupt `workers/<key>.json` ⇒ that single worker is marked `WorkerStatus::Unrecoverable`, loudly reported (and a `NonRestorableNotice` emitted), while the rest of the bundle restores. Any *other* partial condition (two corrupt worker files, corrupt `manager.json`, corrupt `ui.json`, manifest/section disagreement) escalates to `Refused`. "Loudly" means a UI status line **and** a journaled UI record **and** a `tracing::warn!`; a `let _ =` swallow is prohibited.
- Note the deliberate distinction: `ending: InProgress` in the manifest is a *legitimate session state* (a periodic commit that the process never got to finalize), **not** an unclean envelope. It is what makes "killed without a handler" restorable at all, and the loader reports it as `ProcessEnding::HardKillInferred` (§3.4). Refusal keys on **structural** integrity, never on `ending`.

---

## 2. Ground-truth anchor table

Every row was verified by opening the file at the stated lines in HEAD `bbfb893`. The middle column is what the code **says**, not what it is hoped to mean.

### 2.1 The transcript value type (`src/types/wire.rs`)

| `path:line` | Verified fact | Design consequence |
|---|---|---|
| `src/types/wire.rs:7-28` | `enum Message` is `#[serde(tag = "role", rename_all = "lowercase")]` with variants `System{content:String}`, `User{content:String}`, `Assistant{content:Option<String>, reasoning_content:Option<String>, tool_calls:Vec<ToolCall>}`, `Tool{tool_call_id:String, content:String}`. Internally tagged, so the JSON is an **object per message**, and the whole transcript is a bare array. | The tag layout is **reused** by `MessageRecord` (§3.7) so a bundle section is human-inspectable and diff-able against the legacy file. But the type itself is NOT reused — see the next row. |
| `src/types/wire.rs:16` + `:30-41` | **The lossy round-trip trap.** `content` carries `#[serde(default, serialize_with = "serialize_assistant_content")]`, and that function writes `None` as `serialize_str("")`. On read, `""` deserializes back to `Some("")`. So an assistant turn that was `content: None` (tool-call-only turn) becomes `Some("")`: `None → "" → Some("")`. | **Persisted sections MUST NOT contain `Vec<Message>`.** The dedicated `MessageRecord` (§3.7) serializes `content` with an explicit `null` for `None` and no `skip_serializing_if`, so `null` and `""` stay distinct and the round-trip is byte-exact. The "same manager history" requirement fails at the first tool-only assistant turn if this trap is ignored. |
| `src/types/wire.rs:73-80` | `ToolCall { id: String, kind: String (serde key `type`, `default = "function"`), function: ToolFunction }`. `arguments` is reached only through `ToolFunction`. | `ToolCallRecord` (§3.7) mirrors this shape field-for-field, including the `type` key name and its default. |
| `src/types/wire.rs:154-159` | `ToolFunction { name: String, arguments: String }` — `arguments` is an **opaque JSON string**, not a parsed map. | The arguments string is persisted **verbatim** as `String`. Any attempt to `serde_json::from_str` → re-serialize it would re-canonicalize key order/whitespace and break byte-exactness (and would break the `is_malformed` witness). |
| `src/types/wire.rs:88-108` | `ensure_valid_json_arguments` **rewrites** values: empty → `"{}"`, non-object JSON → `{"_raw_value": …}`, malformed → `{"_error":"malformed_arguments","_raw": raw}`. `ToolCall::new` (`:120-133`) and `ToolCall::sanitize_arguments` (`:135-138`) call it. | The loader SHALL never route a record through `ToolCall::new`, `sanitize_arguments`, or `ensure_valid_json_arguments`. A persisted malformed-arguments payload is a **fact about what the model produced** and must come back unchanged; `is_malformed` (`:142-152`) is a computed read-out, not a mutation. |
| `src/manager/context_tests.rs:1071` | "`Message` has no `PartialEq`" — the tests compare through a hand-rolled canonical rendering. | `MessageRecord` SHALL derive `PartialEq, Eq` so an exact round-trip test (`t-014`) can assert equality directly instead of re-implementing a formatter (which is exactly the failure mode D4 prohibits). |

### 2.2 Manager engine and its journals (`src/manager/context.rs`, `src/manager/phase.rs`)

| `path:line` | Verified fact | Design consequence |
|---|---|---|
| `src/manager/context.rs:554-571` | `pub struct ContextEngine` is `#[derive(Debug, Clone)]` — **no serde** — with private fields `messages: Vec<Message>`, `max_context_tokens: usize`, `stats: Option<Arc<HarnessStats>>`, `rebirth_advisory_emitted: bool`, `consecutive_rebirths: usize`, `tool_schema_tokens: usize`. `messages` is the only transcript container. | The engine cannot be serialized wholesale. `ManagerSection` (§3.5) is a **hand-written projection** of exactly the fields that affect the next request: `messages`, `max_context_tokens`, `rebirth_advisory_emitted`, `consecutive_rebirths`, plus the pin witness. `stats` (`Arc`, process-shared) and `tool_schema_tokens` (re-priced from live MCP state at boot) are NOT restorable inputs — see `NonRestorableNotice` in D5. |
| `src/manager/context.rs:592-615` | `set_system_prompt` writes `messages[0]`; `set_goal` writes `messages[1]` (pushing an empty `System` first if the vector is empty). Enforcement is **by index convention only**: nothing asserts the roles. | §4 R-PIN-2: restore SHALL install the vector wholesale and MUST NOT call these setters, because the boot path calls them (`src/ui/session.rs:184`, `:380`) with *freshly composed* text and would silently overwrite the restored pins. |
| `src/manager/context.rs:270-285` | `pinned_prefix_end` finds the **last** index `i ≥ 2` whose content *starts with* `REBIRTH_CHECKPOINT_PREFIX`, matching on `System \| User` **by content, not role** (comment `:266-269`: `perform_rebirth` emits the checkpoint as `User` while transcripts on disk may carry the historic `System` variant), `+ 1`, else `unwrap_or(2)`. | The pinned window is a function of **exact content**. Therefore the loader SHALL never edit advisory/checkpoint prefix text, re-role a message, or re-order the vector (§4 R-PIN-3/4). The persisted `pinned_prefix_end` witness (§3.5) is a *cross-check*: recompute it from the restored vector and refuse on mismatch — that catches any drift this table did not anticipate. |
| `src/manager/context.rs:305-309` + `:314-318` | `INJECTED_ADVISORY_PREFIXES = ["(SYSTEM: CONTEXT BUDGET ADVISORY", "(SYSTEM: CONTEXT LIMIT EXCEEDED", "(SYSTEM: Rebirth checkpoint accepted", REBIRTH_CHECKPOINT_PREFIX]`; `is_injected_advisory` is a `starts_with` scan. The second marker only exists in transcripts written **before** the H7 deletion. *(The Design A contract cites this as `:318-323`; the verified location in HEAD `bbfb893` is the const at `:305` with entries `:306-309`, preceded by the marker-explanation doc block `:287-304`.)* | A restored transcript may legitimately contain a marker the current binary no longer emits. The loader SHALL keep all four byte-identical, and `MessageRecord` SHALL NOT carry a "normalize legacy advisory text" step. |
| `src/manager/context.rs:327` | `pub const ABORTED_TOOL_RESULT: &str = "(aborted)"` — the placeholder synthesized as the `role:"tool"` result for a `tool_calls` entry whose result was dropped or never produced. | This string is part of the durable grammar: a synthesized `"(aborted)"` result persisted in a bundle is indistinguishable from a live one, and `session/` SHALL be the owner of the constant used at load (§8, t-019) so the two paths cannot diverge. |
| `src/manager/context.rs:339-357` | `prune_orphan_tool_messages(Vec<Message>) -> Vec<Message>`: drops `Message::Tool` whose `tool_call_id` has no surviving assistant parent. One half of the invariant. | Reused verbatim by the on-load normalization pass (§8, t-019) — not re-implemented in `session/`. Pruned ids SHALL be recorded in `RepairReport` (loud, D6) rather than silently dropped. |
| `src/manager/context.rs:384-433` | `repair_tool_call_pairs`: (1) prune orphans/duplicates, (2) **move** each result directly under its parent assistant and synthesize `ABORTED_TOOL_RESULT` for missing ones. Doc `:371-374`: nothing else is reordered, so `[0]`/`[1]` keep their positions because they can only be `System`/`User`. | This is the crash-repair the live path never gets to run after a kill (`docs/research_restore_lifecycle.md` §4: `session.rs:689-690` persists the assistant-with-`tool_calls` **before** dispatch). The loader MUST call it, MUST report the synthesized ids, and MUST NOT re-derive its own copy. |
| `src/manager/context.rs:897-903` + `:436-450` | `ensure_tool_call_pairs` wraps the pass and detects change through `pairing_shape` (role tag + `tool_call_id`s, content excluded). Documented **idempotent**: an already-valid transcript returns `false`. | This idempotence is the basis of D6's "unstable normalization ⇒ `Refused`": the loader runs the pass twice; a second pass that changes anything means the input was ambiguous ⇒ refuse. |
| `src/manager/context.rs:988-996` | `save_transcript` = `create_dir_all` + `serde_json::to_string_pretty(&self.messages)` + **non-atomic** `std::fs::write`. Serialized shape is a **bare JSON array**, no version/id/timestamp. | Deleted (D2). The bare-array envelope is why no metadata (session id, pin witness, ending, floors) can be added without a new format — that new format is §3. |
| `src/manager/context.rs:998-1018` | `load_transcript`: parse error ⇒ `tracing::warn!` + `Ok(false)`; `msgs.len() < 2` ⇒ `Ok(false)`; otherwise **wholesale** `self.messages = msgs` with no role check, no repair, no merge. Zero production callers. | Deleted (D2). Its failure mode — corrupt file ⇒ "nothing to restore", silently — is exactly what D6 inverts. The `<2` guard is replaced by the explicit pin verification of §4. |
| `src/manager/phase.rs:117` / `:119` | `TRANSCRIPT_FILE = ".session_transcript.json"`, `UI_TRANSCRIPT_FILE = ".ui_transcript.json"`. | Deleted (D2), replaced by the bundle layout of D3 (`Plan::session_bundle_path()` → `.marmel/session`). |
| `src/manager/phase.rs:298-304` | `Plan::transcript_path()` / `Plan::ui_transcript_path()` = `dir.join(TRANSCRIPT_FILE)` / `dir.join(UI_TRANSCRIPT_FILE)`. | Both deleted; `Plan::session_bundle_path()` is the single accessor (D2). |
| `src/manager/phase.rs:407-421` | `Plan::clear` deletes the forced-phase file, the manager transcript and the UI transcript, each with a `tracing::warn!` and `let _ = remove_file`. | Repointed to the bundle. Semantics preserved: after `Plan::clear` a `--resume` MUST NOT resurrect the session ⇒ commit a `ProcessEnding::Reset` manifest *before* removing the bundle where feasible; otherwise absence of the bundle is the signal (t-006/t-015 decision point). |
| `src/manager/phase.rs:562-569` | Plan archive also removes both transcript files and calls `clear_plan_start()`. | Repointed to the bundle, with `ProcessEnding::PlanArchived` — the "session finished" verdict must be distinguishable from "session interrupted" (`docs/research_restore_lifecycle.md` §5-9). |
| `src/manager/phase.rs:223-235` | `get_plan_start_time()` falls back to disk metadata: takes `created()`/`modified()` as wall clock, computes `elapsed = SystemTime::now() - mod_time`, and returns `(Instant::now() - elapsed, wall)`. | The sanctioned precedent for D5: an `Instant` is **re-anchored from a wall-clock delta**, accepting the approximation. `session::time` generalizes it and documents the error term (§5.6). |

### 2.3 UI surface (`src/ui/transcript.rs`, `src/ui/mod.rs`, `src/ui/tui/mod.rs`)

| `path:line` | Verified fact | Design consequence |
|---|---|---|
| `src/ui/transcript.rs:11-40` | `#[serde(tag = "type")] pub enum UiRecord` — exactly **8** variants: `User{text}`, `Assistant{content:Option,thinking:Option}`, `SteerResponse{text}`, `ToolCall{display}`, `ToolResult{display}`, `TaskCompleted{task_id}`, `TaskFailed{task_id,reason:Option}`, `Status{text}`. All payload types are `String`/`Option<String>`; `PartialEq, Eq` derived. | The variant set is the ceiling of what the journal can express: **no subagent pane text, no streaming partials, no pane state**. Hence D4 (pane strings are the authority) rather than extending `UiRecord` until it lies. Its tag layout is mirrored by `UiRecordRecord`-style persistence only if t-019's §6 decides to journal it; it is never the display authority. |
| `src/ui/transcript.rs:44-46` + API `:49-71` | `UiTranscript` is a newtype over a **private** `records: Vec<UiRecord>`; API is `new/records/is_empty/len/append/clear`. | It can stay an in-memory journal (D2) without exposing internals. A bundle section for it (if any, §6 by t-019) is written by `session/` from `records()`, never by the type itself. |
| `src/ui/transcript.rs:76-86` / `:89-99` | `load`: missing file ⇒ `Ok(default())`, parse error ⇒ hard `Err`, **no version field**, never called on boot (`session.rs:302` = `UiTranscript::new()`). `save`: `create_dir_all` + `to_string_pretty` + **non-atomic** `fs::write`; every one of the 22 call sites is `let _ = …`. | Deleted (D2). The silent-empty-default + all-errors-discarded combination is the fail-open pattern D6 forbids. |
| `src/ui/transcript.rs:113` | `extract_steering_history() -> Vec<(String, String)>` pairs user prompts with `SteerResponse`s; pendingness is delegated to `orchestrator::steer::steering_entry_is_pending` so live history and journal replay share one vocabulary. | **Kept** (D2). Because the journal survives in memory and is restored from its own section, steering history is restored without re-deriving it from pane text. |
| `src/ui/mod.rs:19-37` | `pub struct SubagentDetail` — `#[derive(Debug, Clone, Default)]`, **no serde** — fields: `name`, `task_id: Option<String>`, `prompt`, `started_at: Option<Instant>`, `worked_duration: Duration`, `last_activity_at: Option<Instant>`, `logs: Vec<String>`, `thinking: String`, `content: String`, `is_active: bool`, `context_tokens: usize`. | This is the subagent **pane** model and it is exactly the state the goal demands ("same in-flight subagents" with their visible text). `logs`/`thinking`/`content` are the pane bytes → carried as strings in the UI section (§6, t-019); `started_at`/`last_activity_at` are `Instant` → D5 projection; `worked_duration` becomes an explicit `f64` seconds field so it never depends on two re-anchored instants agreeing. |
| `src/ui/tui/mod.rs:43` | `pub(crate) messages: Vec<String>` — the chat pane's ordered transcript, each entry one string that may contain embedded `\n`. | This is the authority D4 names: `UiSection.chat_messages: Vec<String>` is a 1:1 mirror. Restoring means assigning this vector, not re-rendering records. |

### 2.4 Worker registry identity (`src/orchestrator/workers.rs`)

| `path:line` | Verified fact | Design consequence |
|---|---|---|
| `src/orchestrator/workers.rs:106` | `static WORKERS: LazyLock<DashMap<String, WorkerState>>` — one sharded map holding **active and recently completed** workers, keyed by the correlation key. Codebase rule (`:171-190`, `:196-207`): collect keys into a `Vec` before mutating; never hold an iterator guard while mutating; a panic inside `Drop` during unwind aborts the process. | The bundle is the only cross-process view of this map. A snapshot walker in `session/` SHALL clone keys first (same discipline), and restore SHALL re-insert under the **stored key** (D1 ownership; §5.2) rather than re-registering and letting `claim_worker_slot` mint a new one. |
| `src/orchestrator/workers.rs:112` | `static COMPLETED_SEQ: LazyLock<AtomicU64>` — monotonic sequence handed to completed entries (oldest = smallest), consumed in `ActiveWorkerGuard::drop` (`:165-170`). | `IdentityFloors.completed_seq_floor` (§3.4). Restored completed entries keep their stored `completed_seq`; the static is seeded with `fetch_max(floor)` so a new completion never collides with or mis-orders a restored one. |
| `src/orchestrator/workers.rs:130` + `:132-134` | `static WORKER_ID_SEQ: LazyLock<AtomicU64>` — the unique-id source replacing the old nanosecond pseudo-id; `next_worker_id()` is `fetch_add(1)`, "never reused within a process". | `IdentityFloors.worker_id_seq_floor`. Restoring without seeding this static re-issues ids `0,1,2,…` that the restored keys already occupy ⇒ `claim_worker_slot` disambiguates them with `#N` ⇒ **identity drift** (`docs/research_restore_lifecycle.md` §5-8) and the UI would show different worker keys than before the crash. |
| `src/orchestrator/workers.rs:137-139` + `:243-252` | Key grammar: base key `{agent}-{task}` when a normalized task id exists, else `{agent}-w{worker_id}`; on collision a disambiguated `{base}#{id}` is allocated (`KEY_DISAMBIGUATOR = '#'`), falling back to a UUID past 16 retries. Comment: everything after `#` is an opaque collision handle, which is why matching is never prefix/substring-based. | `<key>` in `workers/<key>.json` is the **exact registry key**, so it must be filename-escaped (`#`→`%23`, `/`→`%2F`) — and the field `WorkerSection.worker_key` remains the authority; the filename is only a handle (§5.2). Because `#` is opaque, `session/` SHALL never parse a key to infer role or task id. |
| `src/orchestrator/workers.rs:103` + `:172-190` | `const MAX_RECENT_COMPLETED: usize = 10`; every guard drop evicts the oldest completed entries beyond the cap. | The live map is lossy by design (≤10 completed). The bundle MAY carry more worker files than the live map holds; restore SHALL respect the cap when re-inserting (oldest-first eviction by `completed_seq`) while `IdentityFloors` keeps the counters monotonic. Loudly report how many completed workers were re-registered vs. dropped. |
| `src/orchestrator/workers.rs:26-40`, `:62-77` | `ActiveWorkerInfo { task_id, agent_name, prompt, started_at: Instant, started_wall: DateTime<Local>, context_tokens, implementation_turns, validation_rounds, latest_validator_feedback: Option<String>, status: String, cancel_token: Option<CancellationToken> }`; `WorkerState { info, last_tokens, completed_seq: Option<u64>, completed_at: Option<Instant>, completed_wall: Option<DateTime<Local>> }`; `is_active()` ⇔ `completed_seq.is_none()`. `status` is a **free-form label** (`"In Progress"`, `"Approved"`, `"Revising"`, `"Aborted"`, `"Failed"`, `"Completed"`). | `WorkerSection` (§3.8) carries both: `status: WorkerStatus` (the machine-readable lifecycle class, which drives cancel/notice routing) **and** `status_label: String` verbatim (the byte shown in the subagent list). `started_wall` is already a durable clock in the live code — the design follows that precedent instead of inventing one. |
| `src/orchestrator/notice.rs:34-39` | `static NEXT_NOTICE_ID: AtomicU64` starts at **1**; `next_notice_id()` returns `format!("notice-{id}")`. | `IdentityFloors.notice_id_floor` = the value of `NEXT_NOTICE_ID` at commit; restore seeds it with `fetch_max(floor)` so a replayed/queued notice id can never alias a fresh one. Queued-but-undelivered notices themselves are in-memory (`PENDING_NOTICES`, `WORKER_INBOXES`: `:76-84`) ⇒ `NonRestorableNotice` (D5, §5.7). |

### 2.5 Where worker history actually lives, and the durable write primitive

| `path:line` | Verified fact | Design consequence |
|---|---|---|
| `src/agents/runner/execution.rs:175-180` | `async fn run_specialist_live_inner(client, agent, ctx: &IsolatedContext, cfg, token)` — the specialist's `ContextEngine` is created **inside** this function at `:250` (`build_specialist_context`) and is a plain local. `IsolatedContext` (`src/agents/mod.rs:129-136`) is the **seed only**: `role_system_prompt`, `brief`, `task_id`, `snippets`, `image_urls`, `audio_urls`, `blueprint`. | **There is no handle to a running worker's history.** `ctx` cannot answer "what has this worker done so far". Therefore the per-worker section of §5 cannot be produced at capture time by *reading* a worker — it MUST be **mirrored out during the run** (§5.5). This is the single largest structural gap in the whole feature. |
| `src/agents/runner/execution.rs:250-258`, `:481-490`, `:524-613`, `:764`, `:1034-1043` | History is mutated by `engine.append(...)` / `engine.replace_last(...)` at many points: snippets block, the assistant reply, nudge injections (`:539`, `:559`, `:584`, `:613`), tool results (`:764`), and compaction/advisory (`:1034-1043`). | Every one of those mutations is a candidate mirror point. A mirror that only samples turn boundaries loses the nudge/compaction state and therefore does not restore "full message history". §5.5 specifies the seam. |
| `src/harness/fs.rs:475-503` | `pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), ToolError>`: unique `O_EXCL` staging name in the **same directory** (`.{stem}.tmp.{pid}.{nanos}.{seq:08}`), `write_all` + `flush`, target-permission preservation, `std::fs::rename`, and `StagedTemp::drop` unlinking the staged file on every failure path. Per-path sharded locks exist at `:334`. | The single write primitive for D3 (D1 owns its use). Because rename is same-directory, the bundle directory MUST be on the same filesystem as `.marmel` — it is by construction. A bundle writer SHALL take the per-path lock for `manifest.json` to serialize concurrent committers. **Amendment B-3 (below §8.4): the atomicity this row buys is atomicity against *process death*, not durability against power loss** — `write_file_atomic` performs no `fsync`/`sync_all` on the file or the parent directory (`src/harness/fs.rs` contains no `sync_all`/`sync_data`), so "the reader sees old-or-new bytes" holds for a Ctrl-C / `kill` / crash, while a power cut can leave bytes the manifest describes off disk (⇒ `byte_len`/`fingerprint` mismatch ⇒ `Refused`, §7.1 "Honest durability limit"). |

---

## 3. Exact Rust types

All types below live in `src/session/format.rs` unless stated otherwise (D1). They are the **complete** durable vocabulary for §1–§5; t-019 adds only the fingerprint function and the UI-section field set it already previews. Wire encoding is `serde_json` with 2-space pretty printing for human inspection (the format is debuggable, and inspection is part of "loudly reported").

### 3.1 Format constants and versioning

```rust
//! `src/session/format.rs` — the only place in the crate that names a session file.

/// Bump on any change to the **envelope/frame** rules (bundle layout, manifest
/// semantics, atomicity point). A reader that does not implement the file's
/// `format_version` MUST refuse (D6) rather than read a subset.
pub const FORMAT_VERSION: u32 = 1;

/// Bump on any change to the **field-level** shape of the types in this module
/// (`Manifest`, `ManagerSection`, `WorkerSection`, `UiSection`, `MessageRecord`…).
pub const SCHEMA_VERSION: u32 = 1;

/// Directory name under `Plan::session_bundle_path()` (D3). The bundle is a
/// directory, never a single file — the manifest-last commit needs several
/// independently atomic renames.
pub const BUNDLE_DIR_NAME: &str = "session";
pub const MANIFEST_FILE: &str = "manifest.json";
pub const MANAGER_FILE: &str = "manager.json";
pub const UI_FILE: &str = "ui.json";
pub const WORKERS_DIR_NAME: &str = "workers";

/// Placeholder for a `role:"tool"` result that was never produced. Owned here so
/// the capture side and the load side can never diverge from each other; mirrors
/// `manager::context::ABORTED_TOOL_RESULT` (`src/manager/context.rs:327`), whose
/// value is pinned by a compile-time equality test in `session` (t-014).
pub const ABORTED_TOOL_RESULT: &str = "(aborted)";
```

`#[serde(deny_unknown_fields)]` appears on **every** durable struct. That is deliberate and is a fail-closed rule (D6): a bundle written by a *newer* release must be **refused**, never partially read. Forward tolerance is provided by `FORMAT_VERSION`/`SCHEMA_VERSION` gating, not by silently ignoring fields — a silently ignored field is how a "restored" session quietly loses state.

### 3.2 `Manifest`, `ManifestMember`, `SectionKind`

```rust
/// The bundle's root document and **atomicity point** (D3): written last, and it
/// is the only file that names the member set. A manifest that parses and whose
/// members all verify describes a set of files that each completed an atomic
/// rename before it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// Envelope rules version (`FORMAT_VERSION`). Mismatch ⇒ `Refused`.
    pub format_version: u32,
    /// Field-shape version (`SCHEMA_VERSION`). Mismatch ⇒ `Refused`.
    pub schema_version: u32,
    /// Opaque per-session identifier, minted when the session's first commit is
    /// made and stable for the bundle's lifetime. Every member header repeats it:
    /// a member from a different session is a hard integrity failure.
    pub session_id: String,
    /// Wall clock at the instant this manifest was committed. Used for
    /// human-facing forensics and for the `NoBundle`/`InProgress` narrative.
    pub committed_wall: DateTime<Local>,
    /// Wall-clock companion of the **process monotonic anchor** that every
    /// `TimePoint::elapsed_secs` in this bundle is measured from (D5, §5.6).
    /// `session::time` samples the anchor once per process and records it here.
    pub anchor_wall: DateTime<Local>,
    /// How the writing process believes it is ending. `InProgress` = a periodic
    /// commit that was never finalized (legitimate, and the common case for
    /// SIGKILL; see §3.4).
    pub ending: ProcessEnding,
    /// Monotonic-counter floors that MUST be re-seeded on restore so ids are
    /// never re-issued (§5.7).
    pub identity_floors: IdentityFloors,
    /// Every member this manifest commits. Order is irrelevant; `rel_path` is the
    /// join key. A file on disk that is not listed here is an orphan and is
    /// ignored (never merged).
    pub members: Vec<ManifestMember>,
}

/// One committed bundle member. The three integrity fields (`rel_path`,
/// `byte_len`, `fingerprint`) are what a reader verifies **before** parsing, so
/// corruption is detected without trusting the file's own header.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestMember {
    /// Which section this member carries.
    pub kind: SectionKind,
    /// Path relative to `Plan::session_bundle_path()`, e.g. `"manager.json"`,
    /// `"ui.json"`, `"workers/coder-t-018%237.json"` (key escaping: §5.2).
    pub rel_path: String,
    /// Exact byte length of the member file as committed. A mismatch ⇒ `Refused`
    /// (or `Unrecoverable`, if and only if the member is a worker file and this is
    /// the only corrupt one — D6).
    pub byte_len: u64,
    /// FNV-1a 64-bit fingerprint over the member file's exact bytes. The function
    /// and its error-reporting shape are specified in §7 (t-019); the field is
    /// declared here because the envelope is defined now.
    pub fingerprint: u64,
    /// Wall clock when this member's payload was captured (not when it was
    /// renamed). Two members captured at different points in the loop can
    /// disagree; this is what makes the disagreement visible.
    pub saved_wall: DateTime<Local>,
    /// Seconds since the anchor of `Manifest.anchor_wall`, for the same instant as
    /// `saved_wall`. Monotonic **within one process**, unlike wall clock, so this
    /// — not `saved_wall` — is what orders members and detects reordering.
    pub saved_elapsed_secs: f64,
}

/// The three section families. `Worker` members carry their registry key inside
/// the section (`WorkerSection.worker_key`), never in the enum.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum SectionKind {
    Manager,
    Ui,
    Worker,
}
```

### 3.3 `SectionHeader`

```rust
/// Frame shared by **every** member file (`manager.json`, `ui.json`, each
/// `workers/<key>.json`): `struct XFile { header: SectionHeader, section: XSection }`.
///
/// The header is a *witness*, not an input: the loader cross-checks it against
/// the manifest and against the payload, and refuses on any disagreement. A
/// section never trusts its own header for integrity — the manifest is the
/// authority (D3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SectionHeader {
    /// Redundant with `Manifest::format_version` on purpose: a member file that
    /// outlives its manifest (orphan) still self-identifies and can be refused
    /// with a precise reason instead of a deserialization error.
    pub format_version: u32,
    pub schema_version: u32,
    /// MUST equal `Manifest::session_id`.
    pub session_id: String,
    pub kind: SectionKind,
    /// MUST equal the corresponding `ManifestMember::saved_wall`.
    pub saved_wall: DateTime<Local>,
    /// MUST equal the corresponding `ManifestMember::saved_elapsed_secs`.
    pub saved_elapsed_secs: f64,
    /// Number of members this section is part of (`1` for Manager/Ui, `1` per
    /// worker file). Combined with `member_index`, this lets the loader detect a
    /// truncated *member set* even if the manifest itself is half-plausible.
    pub member_count: u32,
    pub member_index: u32,
}
```

### 3.4 `ProcessEnding`, `IdentityFloors`

```rust
/// Why the writing process is ending, as **it** knew it. This is a statement
/// about the *session*, not about file integrity: D6's fail-closed rule keys on
/// structural integrity, never on `ending`.
///
/// Writer-side set: everything except `HardKillInferred`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum ProcessEnding {
    /// A periodic (in-loop) commit that was never finalized. The process was
    /// killed without getting to write a terminal manifest — SIGKILL, OOM, a
    /// power cut. **Restorable**, and the normal case, because there is no
    /// signal handler and no shutdown capture in the current binary
    /// (`docs/research_restore_lifecycle.md` §0).
    InProgress,
    /// The session loop returned normally (turn cap, bounds stop, clean exit).
    /// Restoring this is a product decision (t-006), not an integrity one.
    CleanTail,
    /// User abort: Ctrl-C / the TUI abort path / a steering abort.
    UserAbort,
    /// `Plan::clear` / `/reset` deleted the plan and the session artefacts
    /// (`src/manager/phase.rs:407-421`). MUST NOT be resurrected.
    Reset,
    /// The plan was archived (`src/manager/phase.rs:562-569`): the session is
    /// **finished**, which is a different verdict from "interrupted".
    PlanArchived,
    /// Written from the panic hook (`src/main.rs:277-281`), which is pure-sync by
    /// necessity — no `block_on`, `into_inner` on poisoned locks, no panic allowed.
    PanicHook,
    /// Written from a real signal handler. `name` is the signal as spelled by the
    /// handler (`"SIGINT"`, `"SIGTERM"`, `"SIGHUP"`), not a number, so the report
    /// is readable and diff-able.
    Signal { name: String },
    /// **Loader-side sentinel; never written.** Reported by `session::restore`
    /// when it finds `ending == InProgress`: the process never recorded its own
    /// death, so the ending is inferred. Surfacing it as a distinct value keeps
    /// the UI honest ("your previous process was killed; it did not shut down")
    /// without rewriting the bundle bytes (D6).
    HardKillInferred,
}

/// Floors for the process-global monotonic counters that mint identities
/// (`docs/research_restore_lifecycle.md` §5-8: identity drift is the failure mode
/// this exists to prevent).
///
/// Each value is the counter's state at commit time: **every id strictly below it
/// has already been handed out.** Restore MUST raise the corresponding static with
/// `fetch_max(floor)` before any worker is re-registered or any notice is issued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdentityFloors {
    /// `orchestrator::workers::WORKER_ID_SEQ` (`src/orchestrator/workers.rs:130`).
    /// Seeded so `{agent}-wN` keys and `#`-disambiguators cannot be re-issued.
    pub worker_id_seq_floor: u64,
    /// `orchestrator::workers::COMPLETED_SEQ` (`:112`). Seeded so a fresh
    /// completion never sorts before, or ties with, a restored completed entry.
    pub completed_seq_floor: u64,
    /// `orchestrator::notice::NEXT_NOTICE_ID` (`src/orchestrator/notice.rs:34`,
    /// ids spelled `notice-{n}`, origin **1**). Seeded so a notice id can never
    /// alias across the restart.
    pub notice_id_floor: u64,
}
```

### 3.5 `ManagerFile` / `ManagerSection`

```rust
/// On-disk shape of `workers/…` sibling `manager.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerFile {
    pub header: SectionHeader,
    pub section: ManagerSection,
}

/// The exact Manager transcript, plus the engine scalars that change the **next
/// request**. Derived from `ContextEngine` (`src/manager/context.rs:554-571`),
/// which has no serde shape of its own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManagerSection {
    /// The transcript, verbatim and in order, using `MessageRecord` (§3.7) — never
    /// `Vec<Message>` (§2.1, the `serialize_assistant_content` trap).
    pub messages: Vec<MessageRecord>,
    /// `pinned_prefix_end(&messages)` as computed by the writing process
    /// (`src/manager/context.rs:270-285`). Restore recomputes it and MUST refuse on
    /// mismatch (§4 R-PIN-5). `2` is the ordinary value.
    pub pinned_prefix_end: u32,
    /// Witness for the two pins, so a mismatch names *which* pin broke instead of
    /// "the transcript differs". Lengths, not hashes: cheap, and enough to localize.
    pub pin_witness: PinWitness,
    /// Engine budget. Re-read from `Config` at boot today
    /// (`src/ui/session.rs:183`); persisted so a restore can **report** a budget
    /// change rather than silently re-pricing the transcript.
    pub max_context_tokens: usize,
    /// Advisory latch — persists behaviour across the restart (without it the
    /// rebirth advisory re-fires, `src/manager/context.rs:691-696`).
    pub rebirth_advisory_emitted: bool,
    /// Rebirth cooldown counter (`consecutive_rebirths`).
    pub consecutive_rebirths: usize,
    /// Pairing state of the transcript at capture time. A crash lands between the
    /// assistant-with-`tool_calls` write (`src/ui/session.rs:690`) and that round's
    /// tool results, so this is expected to be non-`Balanced` often.
    pub dangling: DanglingToolCallState,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PinWitness {
    /// Byte length of `messages[0].content` (the system prompt, REQ-CORE-001). The
    /// prompt is composed from `Config` + plan state at boot
    /// (`src/ui/session.rs:274-280`) and therefore changes across a restart —
    /// this witness is what proves the restored value is the *old* one.
    pub system_prompt_len: u32,
    /// Byte length of `messages[1].content` (the pinned goal, REQ-CORE-002).
    pub goal_len: u32,
}
```

Not persisted, by decision (each yields a `NonRestorableNotice` instead): `stats: Option<Arc<HarnessStats>>` (process-shared counters, and never attached to the Manager engine anyway — `set_stats` has no live caller), and `tool_schema_tokens` (re-charged from live MCP state at boot; a persisted value would be a *lie* if the MCP set changed).

### 3.6 `UiSection`

```rust
/// On-disk shape of `ui.json`. **Pane text is the authority** (D4): no
/// replay-to-rebuild. The full field-by-field spec (subagent panes, composer
/// draft, scroll/focus/toggle state, and which of them are restorable at all) is
/// §6, written by t-019. The two fields below are fixed here because §1 D4 and
/// §2.3 pin them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiFile {
    pub header: SectionHeader,
    pub section: UiSection,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    /// The chat pane, one entry per pane line, **exactly as displayed**
    /// (1:1 with `TuiRenderer.messages: Vec<String>`, `src/ui/tui/mod.rs:43`).
    /// Includes the prefixes the pane spells (`"User: "`, `"[Tool Call] "`, …) and
    /// the local `[CLI]` lines that the semantic journal never records.
    ///
    /// Restore SHALL assign this vector and SHALL NOT regenerate any entry from a
    /// `UiRecord`.
    pub chat_messages: Vec<String>,
    /// Exact status-line text (`TuiRenderer.status_line`), so the status bar is
    /// byte-identical instead of resetting to `"Ready"`.
    pub status_line: String,
}
```

### 3.7 `MessageRecord` — the loss-proof transcript record

```rust
/// Durable mirror of the wire `Message` tag layout (`src/types/wire.rs:7-28`) that
/// survives a JSON round-trip **exactly**. Never reuse `Message` for persistence:
/// `serialize_assistant_content` (`src/types/wire.rs:30-41`) writes `None` as
/// `""`, and `""` reads back as `Some("")`, so a tool-only assistant turn is not
/// recoverable through the wire type (§2.1).
///
/// Difference from `Message`, deliberately: `content` on an assistant record is
/// serialized **without** `skip_serializing_if` and without any `serialize_with`,
/// so `None` is written as JSON `null` and read back as `None`. `null` and `""`
/// are therefore distinct, permanently.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "lowercase", deny_unknown_fields)]
pub enum MessageRecord {
    #[serde(rename = "system")]
    System { content: String },
    #[serde(rename = "user")]
    User { content: String },
    #[serde(rename = "assistant")]
    Assistant {
        /// `null` = the model produced no content (tool-only turn).
        /// `""`  = the model produced an empty content string. Not the same fact.
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reasoning_content: Option<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        tool_calls: Vec<ToolCallRecord>,
    },
    #[serde(rename = "tool")]
    Tool {
        tool_call_id: String,
        content: String,
    },
}

/// Verbatim mirror of `ToolCall` (`src/types/wire.rs:73-80`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCallRecord {
    pub id: String,
    /// Wire key is `"type"`, default `"function"` — same spelling as the wire type
    /// so a bundle diff against the legacy transcript reads cleanly.
    #[serde(rename = "type", default = "default_tool_type")]
    pub kind: String,
    pub function: ToolFunctionRecord,
}

fn default_tool_type() -> String {
    "function".to_string()
}

/// `arguments` is an **opaque JSON string** (`src/types/wire.rs:154-159`) and is
/// persisted verbatim. Two prohibited operations, both of which silently rewrite
/// history: `serde_json::from_str` → re-serialize (re-canonicalizes), and any
/// route through `ensure_valid_json_arguments` / `ToolCall::sanitize_arguments`
/// (`src/types/wire.rs:88-108`, `:135-138`), which *mutates* malformed arguments
/// into `{"_error":"malformed_arguments",…}`. A record that already carries that
/// payload keeps it unchanged, and `is_malformed` stays a computed read-out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolFunctionRecord {
    pub name: String,
    pub arguments: String,
}

impl MessageRecord {
    /// In-memory conversion, field by field. MUST NOT be implemented by
    /// serializing a `Message` to JSON and reading it back — that path runs
    /// through `serialize_assistant_content` and destroys the `None`/`""`
    /// distinction on the way in.
    pub fn from_wire(msg: &Message) -> Self { … }

    /// Reverse conversion for the request builder. `None` stays `None`, so the
    /// wire bytes a restored session sends are the bytes the pre-crash process
    /// would have sent.
    pub fn into_wire(self) -> Message { … }
}
```

### 3.8 Worker section types

```rust
/// On-disk shape of `workers/<key>.json`. One file per worker, one worker per
/// file — never an array, so a single corrupt worker can be isolated (D6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerFile {
    pub header: SectionHeader,
    pub section: WorkerSection,
}

/// Everything needed to re-insert an identical entry into the live `WORKERS` map
/// (`src/orchestrator/workers.rs:106`) and to rebuild the worker's conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSection {
    /// The **exact** registry key, including any `#…` disambiguator
    /// (`src/orchestrator/workers.rs:137`, `:243-252`). This field, not the
    /// filename, is the authority; the filename is an escaped handle (§5.2).
    /// Restore inserts under this key and never re-registers.
    pub worker_key: String,
    /// The value drawn from `WORKER_ID_SEQ` when this worker registered, recorded
    /// so `{agent}-wN` keys and disambiguators reproduce exactly.
    pub worker_id_seq_value: u64,
    /// Agent role. Reuses the crate's role vocabulary
    /// (`agents::Agent`, `#[serde(rename_all = "snake_case")]`,
    /// `src/agents/mod.rs:37-39`) — role names are a shared vocabulary, not a
    /// session format, so D1 does not fork them.
    pub role: Agent,
    /// Normalized task id binding (`Option`; `None` for task-less workers, whose
    /// key is `{agent}-wN`).
    pub task_id: Option<String>,

    /// Lifecycle class used by cancel/notice/`has_active_workers()` routing.
    pub status: WorkerStatus,
    /// The live free-form label, verbatim (`ActiveWorkerInfo.status`:
    /// `"In Progress"`, `"Revising"`, `"Approved"`, `"Aborted"`, `"Failed"`,
    /// `"Completed"`) — the bytes the subagent list renders.
    pub status_label: String,

    /// Clocks (D5). `started` mirrors `started_at`/`started_wall`; `completed`
    /// mirrors `completed_at`/`completed_wall`; `last_activity` mirrors
    /// `SubagentDetail.last_activity_at`.
    pub started: TimePoint,
    pub completed: Option<TimePoint>,
    pub last_activity: Option<TimePoint>,
    /// Accumulated working time as an explicit number of seconds, so it never has
    /// to be computed as a difference of two re-anchored instants (`SubagentDetail::worked_duration`).
    pub worked_secs: f64,

    /// Completed-ring identity (`WorkerState::completed_seq`); `None` ⇔ active.
    /// Restore keeps the stored value and must not re-draw it.
    pub completed_seq: Option<u64>,

    /// Token counters: live `context_tokens` and the post-completion
    /// `last_tokens` (`src/orchestrator/workers.rs:66`).
    pub context_tokens: usize,
    pub last_tokens: usize,
    /// Budget/turn counters.
    pub counters: WorkerCounters,

    /// The delegation seed — required to re-advertise the same task, and the only
    /// thing `IsolatedContext` holds today (`src/agents/mod.rs:129-136`).
    pub brief: String,
    pub snippets: Vec<String>,
    pub image_urls: Vec<String>,
    pub audio_urls: Vec<String>,
    pub recursion_granted: bool,

    /// The worker's **full** conversation, in order (§5.5). Not the seed.
    pub history: Vec<MessageRecord>,
    /// Provenance of `history`: whether it was mirrored from the live engine or
    /// is only the seed. A `SeedOnly` history is reported, never presented as an
    /// exact restoration (D6 "loudly").
    pub history_source: HistorySource,

    /// Where the worker's tool-call pairing stood at capture instant (§5.4).
    pub dangling: DanglingToolCallState,

    /// Populated iff `status == Unrecoverable` (D6's only tolerated partial).
    pub unrecoverable_reason: Option<String>,
    /// Projections that lost a capability for this worker (D5): its cancellation
    /// token, its task handle, its live PTY, its guard, its queued notices.
    pub non_restorable: Vec<NonRestorableNotice>,
}

/// Lifecycle class. Deliberately orthogonal to `status_label`: the label is display
/// bytes, this is control state. `Unrecoverable` is produced only by the loader
/// (D6) and never written by a healthy capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkerStatus {
    /// Active in the pre-crash process (`completed_seq == None`).
    InProgress,
    Completed,
    Failed,
    Aborted,
    /// A live status string this build does not recognize. Preserved verbatim in
    /// `status_label`; never coerced into one of the four known classes.
    Unknown,
    /// Loader verdict: this worker's file failed verification and was the only
    /// corrupt member (D6). Its history is gone; it is reported loudly and its
    /// pane identity is kept so the UI still shows *that it existed*.
    Unrecoverable,
}

/// Budget/turn counters, carried verbatim from `ActiveWorkerInfo`
/// (`src/orchestrator/workers.rs:32-34`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCounters {
    pub implementation_turns: usize,
    pub validation_rounds: usize,
    pub latest_validator_feedback: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistorySource {
    /// Mirrored out of the live engine turn by turn (§5.5). Exact.
    LiveMirror,
    /// Only the seed was recoverable (a worker that never reached the first
    /// mirror point). Reported, never silently upgraded to a "history".
    SeedOnly,
    /// History intentionally not captured (e.g. a worker already completed and
    /// evicted from the mirror ring). `history` is then empty.
    NotCaptured,
}
```

### 3.9 Pairing state, time projection

```rust
/// The assistant `tool_calls` ↔ `role:"tool"` pairing state at capture instant —
/// the invariant `manager::context` documents in both directions
/// (`src/manager/context.rs:361-382`). Recorded so a restore can *report* what it
/// healed instead of discovering a provider 400 at the first request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum DanglingToolCallState {
    /// Every `tool_calls` entry has its result, and no orphan results exist.
    Balanced,
    /// An assistant turn has `tool_calls` entries with no result yet — the exact
    /// shape left by `src/ui/session.rs:690` persisting the assistant before the
    /// round's dispatch. `dispatched` says whether the tool began (its result may
    /// exist elsewhere, e.g. in a PTY whose process survived the kill — §5.7).
    Dangling { calls: Vec<DanglingCall> },
    /// `role:"tool"` results whose parent is gone. `prune_orphan_tool_messages`
    /// (`src/manager/context.rs:339`) removes them at load; the ids are reported.
    OrphanedResults { tool_call_ids: Vec<String> },
    /// Capture already applied the `ABORTED_TOOL_RESULT` placeholder, recording
    /// which ids it synthesized. The loader does not re-synthesize these.
    Healed { aborted_tool_call_ids: Vec<String> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DanglingCall {
    pub tool_call_id: String,
    pub function_name: String,
    /// Verbatim arguments, so the report can show the call that never completed.
    pub arguments: String,
    pub dispatched: bool,
}

/// Durable projection of one `std::time::Instant` (D5). `session::time` (§5.6) is
/// the only module allowed to build or read one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TimePoint {
    /// Seconds since the **capture process's monotonic anchor** (whose wall-clock
    /// position is `Manifest::anchor_wall`). Monotonic within one process; the only
    /// reliable ordering witness.
    pub elapsed_secs: f64,
    /// Absolute wall clock sampled together with `elapsed_secs`. Used to re-anchor
    /// an `Instant` in the new process, following the sanctioned precedent
    /// `Plan::get_plan_start_time()` (`src/manager/phase.rs:223-235`), and to print
    /// an honest timestamp.
    pub wall: DateTime<Local>,
}
```

### 3.10 `RestoreOutcome` and `RepairReport`

```rust
/// Result of `session::restore`. **In-memory only** — never serialized: it is the
/// loader's verdict about a bundle, and `ProcessEnding::HardKillInferred` exists
/// precisely because this verdict must not be written back (D6: bundle bytes
/// untouched).
#[derive(Debug)]
pub enum RestoreOutcome {
    /// No `manifest.json` under `Plan::session_bundle_path()`. Not an error. If
    /// legacy journals are present, the caller MUST be told they are not
    /// restorable (D2).
    NoBundle,
    /// Fail-closed (D6): the bundle was **not** read into state and **no byte of
    /// it was modified**.
    Refused { refusal: Refusal, report: RepairReport },
    /// Full restore. `ending` is the *derived* ending (`InProgress` reported as
    /// `HardKillInferred`).
    Restored { session: RestoredSession, ending: ProcessEnding, report: RepairReport },
    /// D6's single tolerated partial: one corrupt worker file. That worker is
    /// present with `WorkerStatus::Unrecoverable`; everything else restored.
    PartiallyRestored {
        session: RestoredSession,
        ending: ProcessEnding,
        unrecoverable_workers: Vec<String>,
        report: RepairReport,
    },
}

/// Machine-readable refusal, so the UI message and the log carry the same numbers
/// a reader would compute by hand.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub code: RefusalCode,
    pub message: String,
    /// Offending member, when the refusal is member-scoped.
    pub member: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RefusalCode {
    FormatVersion,
    SchemaVersion,
    UnknownField,
    Json,
    MissingMember,
    ByteLen,
    Fingerprint,
    PinViolation,
    NormalizationUnstable,
    HeaderManifestDisagreement,
    Io,
}

/// What the loader had to do to make the transcript sendable, and everything it
/// could not restore. One object, three destinations: UI status line, journaled UI
/// record, `tracing::warn!` — never a discarded `let _` (D6).
///
/// Field *semantics* are finalized in §8 (t-019); the shape is declared here
/// because §3's types already reference it and §4/§5 already assert on it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepairReport {
    pub pins_verified: bool,
    /// Recomputed vs. persisted `ManagerSection::pinned_prefix_end`.
    pub pinned_prefix_end_expected: Option<u32>,
    pub pinned_prefix_end_actual: Option<u32>,
    /// Counts from the normalization passes (§8):
    pub orphan_tool_results_pruned: usize,
    pub aborted_results_synthesized: usize,
    pub tool_results_regrouped: usize,
    /// MUST be `2` on any path that ran normalization, and `normalization_stable`
    /// MUST be `true` — otherwise the outcome is `Refused` (D6).
    pub normalization_passes: u8,
    pub normalization_stable: bool,
    pub manager_messages_before: usize,
    pub manager_messages_after: usize,
    pub ui_lines_restored: usize,
    pub workers_restored: usize,
    pub workers_unrecoverable: Vec<String>,
    /// Completed entries dropped because the live ring holds ≤10
    /// (`src/orchestrator/workers.rs:103`).
    pub completed_dropped_by_ring_cap: usize,
    /// Every D5 projection that lost a capability.
    pub notices: Vec<NonRestorableNotice>,
    pub warnings: Vec<String>,
}
```

### 3.11 `NonRestorableNotice` (D5)

```rust
/// A first-class, user-visible statement that a capability did not survive the
/// restart. Emitted by every lossy projection; collected into `RepairReport`, the
/// UI, and the journaled UI record. Not a log line, and never a silent re-mint.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NonRestorableNotice {
    pub kind: NonRestorableKind,
    /// What it applies to: a worker key, a task id, or the session.
    pub subject: String,
    /// One English sentence naming the consequence and the expected user action.
    pub detail: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum NonRestorableKind {
    /// `CancellationToken` (`ActiveWorkerInfo::cancel_token`) — the restored worker
    /// cannot be cancelled through the old token; a new one is minted by the
    /// restore path and the worker is reported as non-cancellable-until-rearmed.
    CancellationToken,
    /// `JoinHandle` — no task exists to join or abort; teardown is cooperative-only
    /// (`docs/research_restore_lifecycle.md` §5-3).
    JoinHandle,
    /// PTY / process group — OS children may have survived the kill and may still
    /// be mutating the workspace (`src/harness/pty.rs`, no `Pdeathsig`).
    PtyProcess,
    /// RAII guard (`ActiveWorkerGuard`) — its completion/eviction side effects will
    /// not run.
    DropGuard,
    /// Queued-but-undelivered steering notices (`PENDING_NOTICES`, `WORKER_INBOXES`,
    /// `src/orchestrator/notice.rs:76-84`).
    PendingNotice,
    /// `HarnessStats` intervention counters.
    HarnessStats,
    /// `tool_schema_tokens` re-priced from live MCP state.
    ToolSchemaCharge,
    /// A worker whose `history` is `SeedOnly`/`NotCaptured`, or `Unrecoverable`.
    WorkerHistory,
    Other,
}
```

---

## 4. Pin rules (REQ-CORE-001 / REQ-CORE-002)

The two pins are `messages[0]` (system prompt, **REQ-CORE-001** KV-cache prefix preservation, `src/manager/context.rs:5-7`) and `messages[1]` (immutable goal, **REQ-CORE-002**, `:8-9`). Both are enforced today **by index convention only** — no code anywhere asserts the roles (§2.2). A restore that breaks a pin does not fail loudly; it changes what compaction keeps, what the model's prefix cache hits, and what the next request costs.

### R-PIN-1 — Record the pins verbatim

- The capture side SHALL record `messages[0]` and `messages[1]` as `MessageRecord::System { content }` and `MessageRecord::User { content }`, byte-for-byte, with **no** trimming, re-wrapping, newline normalization, truncation, redaction, or template re-rendering.
- The pinned pair SHALL be exactly what `ContextEngine::messages()` (`src/manager/context.rs:633`) returned at capture instant — not what `build_manager_context` (`src/ui/session.rs:182`) or `DelegationRequest` would produce now.
- The capture side SHALL also write `pin_witness.system_prompt_len` / `goal_len` and the `pinned_prefix_end` witness (§3.5) computed from the same vector.

### R-PIN-2 — Restore MUST NOT route through `set_system_prompt` / `set_goal`

- `session::restore` SHALL install the restored vector **wholesale** through one new engine seam (working name `ContextEngine::install_restored_messages(Vec<MessageRecord>)`, owned by `session/`'s contract with `manager::`, t-010). It MUST NOT call `set_system_prompt` (`src/manager/context.rs:592`) or `set_goal` (`:604`) at any point during or after installation.
- **Why, concretely:** the boot path unconditionally calls `ctx.set_system_prompt(system)` inside `build_manager_context` (`src/ui/session.rs:184`) and `ctx.set_goal(goal)` once a goal exists (`:380`). Both write by index. So a naive restore that loads the transcript and then lets the normal boot sequence run gets `[0]` overwritten with the **freshly composed** prompt (`load_system_prompt_with_plan_and_warning`, `session.rs:274-280` — composed from `Config` + plan state, therefore different across a restart) and `[1]` overwritten with the CLI argument or newly typed input. Either ordering is wrong: load-then-set clobbers the restored pins, set-then-load wipes the just-set `[0]` (§2.2).
- Therefore the resumed boot path SHALL **skip** the two setters on the restore branch and SHALL NOT recompute the system prompt at all. Where the fresh composition is genuinely needed (e.g. a plan that changed on disk while the process was dead), the difference SHALL be surfaced as a `RepairReport::warnings` entry and `NonRestorableNotice { kind: Other }`, not resolved by silently replacing the pinned bytes.
- `set_goal`'s empty-`System` padding (`:604-611`: pushes `System { content: String::new() }` when the vector is empty) SHALL never run on the restore branch: a restored session with an empty system prompt is a **refusal**, not a thing to paper over.

### R-PIN-3 — No re-role, no re-order

- The loader SHALL NOT change any message's role variant, and SHALL NOT reorder the vector — with exactly one exception: the documented re-grouping performed by `repair_tool_call_pairs` (§8), which moves only `Tool` results under their parent assistant and provably leaves `[0]`/`[1]` in place because those can only be `System`/`User` (`src/manager/context.rs:371-374`).
- **Why content-matching makes this load-bearing:** `pinned_prefix_end` (`src/manager/context.rs:270-285`) takes the **last** `i ≥ 2` whose content starts with `REBIRTH_CHECKPOINT_PREFIX`, matching `System | User` **by content** precisely because on-disk transcripts may carry the checkpoint as `System` while `perform_rebirth` emits it as `User` (`:266-269`). If a restore "modernized" a `System` checkpoint into `User`, or re-ordered it, the pinned window would move — and `compact()` (`src/manager/context.rs:751`) keeps `messages[..prefix_end]` unconditionally, so a moved window silently changes which turns survive the next compaction. Worse, `perform_rebirth` (`:935`) re-reads the pins **positionally** and silently substitutes `String::new()` when `[0]` is not `System` or `[1]` is not `User` (`:938-947`): a re-ordered restore therefore collapses the next rebirth into a system-less, goal-less transcript with no error anywhere.
- Consequently, the historic `System`-variant checkpoint SHALL be preserved as `MessageRecord::System`, even though the current binary would have emitted `User`. A `warnings` entry MAY note the legacy shape; the bytes SHALL NOT change.

### R-PIN-4 — The loader never rewrites advisory prefix text

- The four markers in `INJECTED_ADVISORY_PREFIXES` (`src/manager/context.rs:305-309`) — `(SYSTEM: CONTEXT BUDGET ADVISORY`, `(SYSTEM: CONTEXT LIMIT EXCEEDED`, `(SYSTEM: Rebirth checkpoint accepted`, and `REBIRTH_CHECKPOINT_PREFIX` (`:92`) — are the **only** way the runtime recognizes its own injected messages (`is_injected_advisory`, `:314-318`). `is_injected_advisory` is a pure `starts_with` scan, so a leading space, a re-wrapped line, a re-spelled marker, or a locale-translated prefix makes the message look like a genuine user instruction.
- The loader SHALL therefore treat every such prefix as opaque bytes: no trim, no re-wrap, no Unicode normalization, no template re-rendering. This is what keeps `perform_rebirth` from collapsing a boilerplate advisory into the `messages[2]` slot instead of the last genuine instruction (bug M6, the `skip(2).rev().find_map` scan at `src/manager/context.rs:949-957`).
- The loader SHALL keep markers the current binary no longer emits — notably `SYSTEM: CONTEXT LIMIT EXCEEDED`, which exists only in transcripts written before the H7 deletion — and SHALL NOT prune, merge, or reword them. Pruning them would be a silent edit to the manager's message history, which the goal forbids.
- Note the interaction with R-PIN-3: because the pinned window is found by content scan, "cleaning up" advisory text can silently move `pinned_prefix_end`. The witness check in R-PIN-5 is what catches it.

### R-PIN-5 — A pin mismatch is a hard `Refused`

The verification procedure SHALL run before any state is installed, and SHALL be the first structural check after the manifest members verify:

```text
1. len(messages) >= 2                                    else Refused(PinViolation, "transcript shorter than the pinned prefix")
2. messages[0] is MessageRecord::System { content }       else Refused(PinViolation, "messages[0] is <role>, must be system")
   content.len() == pin_witness.system_prompt_len         else Refused(PinViolation, "system prompt length N != witness M")
3. messages[1] is MessageRecord::User { content }         else Refused(PinViolation, "messages[1] is <role>, must be user")
   content.len() == pin_witness.goal_len                  else Refused(PinViolation, "goal length N != witness M")
4. recompute pinned_prefix_end(messages)
     == section.pinned_prefix_end                         else Refused(PinViolation, "pinned window moved: X != Y")
5. recompute pin_witness from messages                    (belt-and-braces: proves 2 and 3 on the vector actually installed)
```

- Every failure is `RestoreOutcome::Refused { code: PinViolation, … }` (D6). There is **no** pin repair, no "adopt the freshly composed prompt", no re-pinning from the CLI goal, and no partial restore that keeps the messages but drops the witness. The bundle bytes SHALL be untouched and the mismatch numbers SHALL be printed.
- Rule 4 is the safety net for everything §2.2 and R-PIN-3/4 do not anticipate: any content drift in the checkpoint/advisory markers, any accidental re-role, and any re-order all surface as a moved pinned window.
- Rationale for zero tolerance: a session restored past a pin violation *looks* correct — the panes are full, the manager answers — while REQ-CORE-001's prefix identity and REQ-CORE-002's guarantee are both already gone. That is the exact class of silent degradation D6 exists to eliminate.

### R-PIN-6 — Post-restore compaction behaviour is unchanged

- After a successful restore, `should_compact()`, `compact()`, `should_advise_rebirth()` and `perform_rebirth()` SHALL run against the restored vector with no special-casing. The restore is invisible to them — which is only true because R-PIN-2/3/4/5 guarantee the vector's shape.
- `max_context_tokens` is re-read from `Config` at boot today. If it differs from the persisted value, the loader SHALL NOT silently adopt either one: it SHALL report the pair in `RepairReport::warnings` (the pinned prefix may already exceed the new 70 % target, and `compact()` (`src/manager/context.rs:751`) would then honestly report `CompactionOutcome::TargetUnreachable` — arm at `:547` — instead of claiming success on an over-budget transcript).

---

## 5. Per-worker section spec (`workers/<key>.json`)

### 5.1 What the section promises

One file per worker, one worker per file. The section MUST carry enough to re-insert an entry into the live `WORKERS` map (`src/orchestrator/workers.rs:106`) that is **indistinguishable from the pre-crash entry** in four respects:

1. **identity** — same registry key, same `worker_id_seq_value`, same `completed_seq`;
2. **history** — the worker's full `Vec<MessageRecord>` conversation, not its seed;
3. **status and counters** — the lifecycle class, the display label, the turn/round counters and token counts the subagent list shows;
4. **clocks** — `started` / `completed` / `last_activity` / `worked_secs` such that elapsed-time displays continue from where they stood (D5).

Everything the section *cannot* carry (cancellation token, task handle, PTY, guard, queued notices) is represented by a `NonRestorableNotice` (§3.11) — never by a silently re-minted equivalent (D5).

### 5.2 Key → filename, and which one is the authority

- `<key>` in `workers/<key>.json` SHALL be the exact registry key from `WorkerSection.worker_key`, with **percent-escaping** applied for characters that are unsafe in a path component: `#` → `%23` and `/` → `%2F` (plus `%` → `%25`, first). The base-key grammar is `{agent}-{task}` or `{agent}-w{N}`, and collisions append `#{id}` with `KEY_DISAMBIGUATOR = '#'` (`src/orchestrator/workers.rs:137`, `:243-252`), so `#` genuinely appears in keys and must be escaped.
- **The field is the authority; the filename is a handle.** The loader SHALL take the key from `WorkerSection.worker_key` after verification and SHALL NEVER parse a filename (or a key) to infer the role, the task id, or the collision handle — matching on keys by prefix or substring is already prohibited in the live code for exactly this reason (`:137-139`: everything after `#` is an opaque collision handle).
- The manifest lists the escaped `rel_path`; the loader SHALL refuse (`RefusalCode::MissingMember`) if an escaped path in the manifest does not resolve to the file whose `worker_key` decodes to the expected key, so a hand-edited bundle cannot swap two workers.

### 5.3 Identity, binding, status, counters

| `WorkerSection` field | Live source (verified) | Restore action |
|---|---|---|
| `worker_key` | `WORKERS` map key (`workers.rs:106`), minted at `:243-252` | Insert under this exact key. SHALL NOT call `register_active_worker_with_token` / `claim_worker_slot` for a restored worker: that path draws a **new** `WORKER_ID_SEQ` id and may disambiguate the key, which is the identity drift of `docs/research_restore_lifecycle.md` §5-8. |
| `worker_id_seq_value` | the id drawn by `next_worker_id()` (`:132-134`) | Cross-check that the key's `-wN` suffix / `#N` handle is consistent with it; also contributes to `identity_floors` (§5.7). |
| `role` | `ActiveWorkerInfo::agent_name` (`:27`) via `agents::Agent` snake_case | Rebuild the specialist by role (t-006's re-launch policy). Never derived from the key. |
| `task_id` | `ActiveWorkerInfo::task_id` (`:26`), already normalized by `task_id::normalize_task_id` (`:235-237`) | Re-establish the plan-task binding so check-off and cancel routing still target the same task. `None` stays `None` (a `-wN` worker); a `Some` value SHALL be re-validated with the same grammar, and a violation is a `Refused`, not a coercion. |
| `status` | derived from `WorkerState::completed_seq` (`is_active()`, `:78`) + `ActiveWorkerInfo::status` | Sets `completed_seq = None` for `inProgress` so the entry is active to `has_active_workers()` and to every cancel path; `completed*` for terminal classes. |
| `status_label` | `ActiveWorkerInfo::status` free-form string (`:36`) | Assigned verbatim, so the subagent list shows the pre-crash words (`Revising`, `Approved`, …). An unrecognized label ⇒ `status = Unknown` **and** the verbatim label kept. |
| `completed_seq` | `COMPLETED_SEQ` draw in `ActiveWorkerGuard::drop` (`:165-170`) | Kept verbatim; the static is raised to the floor (§5.7) so a later completion cannot tie with or precede it. |
| `context_tokens`, `last_tokens` | `ActiveWorkerInfo::context_tokens` (`:31`) + `WorkerState::last_tokens` (`:66`) | Assigned verbatim so the status bar's context-token readout does not reset to 0. |
| `counters.implementation_turns`, `validation_rounds`, `latest_validator_feedback` | `ActiveWorkerInfo` (`:32-35`) | Assigned verbatim: these gate the fix-loop/verdict behaviour and are shown in the subagent list (`docs/research_restore_lifecycle.md` §2.4 notes the prior art printed them but could not restore them). |
| `brief`, `snippets`, `image_urls`, `audio_urls`, `recursion_granted` | `DelegationRequest` (`src/agents/mod.rs:105-125`) / `IsolatedContext` (`:129-136`) | The **seed**. Restored so a re-launched worker re-advertises the identical task; never used as a substitute for `history` (that is `SeedOnly`, §3.8, and must be reported). |

### 5.4 Message history and pending/dangling tool-call state

- `history: Vec<MessageRecord>` SHALL be the worker's complete engine transcript at capture instant — the system prompt the specialist was built with, the brief, the snippets block (`src/agents/runner/execution.rs:253-258`), every assistant turn with its `reasoning_content` and `tool_calls`, every `role:"tool"` result, every nudge injection (`:539`, `:559`, `:584`, `:613`, `:764`), and any compaction/advisory outcome (`:1034-1043`). Order and count SHALL match the live engine exactly; `MessageRecord` keeps `content: None` vs `Some("")` distinct (§3.7) and `arguments` verbatim (§2.1).
- `dangling: DanglingToolCallState` (§3.9) records the pairing state at capture: the tool calls the worker requested but whose results had not landed when the process died, the orphaned results (if any), and any placeholder already synthesized. On restore:
  - `Balanced` → nothing to do.
  - `Dangling { calls }` → the loader runs the same normalization pass as the manager section (§8) and reports each id that gets `ABORTED_TOOL_RESULT`. `dispatched: true` combined with a surviving PTY SHALL additionally raise `NonRestorableKind::PtyProcess`: the subprocess may still be mutating the workspace, and re-applying the same tool call is a double-apply (`docs/research_restore_lifecycle.md` §5-10).
  - `OrphanedResults` → pruned by `prune_orphan_tool_messages` at load, ids reported.
  - `Healed` → not re-synthesized (the placeholders are already in `history`).
- Worker history SHALL be normalized by the **same** passes as the manager history (§8) — one grammar, one owner — and the same stability rule applies: a second pass that changes anything escalates to `Refused`. A worker whose history is not pair-valid after one pass SHALL NOT be presented as "restored exactly".

### 5.5 The gap this section closes: worker history must be mirrored **during the run**

- Today the specialist's `ContextEngine` is a **local** inside `run_specialist_live_inner` (`src/agents/runner/execution.rs:175-180`, built at `:250` by `build_specialist_context`) and nothing outside that function can reach it. `IsolatedContext` is the seed only. There is **no** `save_transcript` anywhere in `src/agents/`, and no `&mut` handle to snapshot — the prior art therefore re-ran workers from the initial request with zero accumulated history (`31f4e90:src/orchestrator/mod.rs:432`, `:471`).
- Therefore the history SHALL be **mirrored out as it is produced**, and the mirror is owned by `session/worker_journal` (D1). The normative shape:
  1. A `WorkerJournal` handle is created when the worker registers (keyed by `worker_key`) and is threaded into `run_specialist_live_inner` alongside `ctx` — the same scope mechanism `CURRENT_WORKER_TOKEN` already uses (`execution.rs:165-171`) is the precedent for reaching code that has no parameter path.
  2. The mirror SHALL observe the engine at **every** mutation point, i.e. it wraps/parallels `engine.append(...)` and `engine.replace_last(...)` rather than sampling at turn boundaries. Sampling at boundaries loses nudge injections and mid-round tool results, and those are part of "full message history".
  3. `replace_last` SHALL be mirrored as a *replace* of the last mirrored record, not an append, or the mirror drifts longer than the engine (it would also break the `pairing_shape` cross-check).
  4. Compaction/rebirth inside the worker (`execution.rs:1034-1043`) SHALL collapse the mirror to match the engine, so the mirror never re-expands history the worker itself discarded.
  5. The mirror is authoritative for `history_source: LiveMirror`. A worker that registered but never reached the first mirror point SHALL be written with `history_source: SeedOnly` and an empty `history`, and that fact SHALL appear in `RepairReport` (it is a partial result, not an exact restore).
- The mirror is a `Vec<MessageRecord>` (or an append-only log that folds to one) — it is **not** the engine, and it does not change how the engine runs. `session/` never reaches into a live engine to pull state; state flows out through the mirror, once per mutation, cheaply, and is committed by the bundle protocol (§6, t-019) at the triggers t-006 defines.
- **Sizing note (normative, not optional):** worker mirrors are the largest payload in the bundle — one full transcript per in-flight worker. The commit protocol MUST therefore be able to write worker members independently of `manager.json` (which D3's split-file layout makes possible) and MUST NOT serialize a bundle by re-serializing every mirror on every manager turn.

### 5.6 Time projections (D5 rules)

`session::time` is the sole owner. Rules:

- **T1 — capture.** For each `Instant` worth persisting, record a `TimePoint { elapsed_secs, wall }`: `elapsed_secs` = seconds since the process's monotonic anchor (`Instant::now() - anchor`, as `f64`), and `wall` = `chrono::Local::now()` sampled in the same statement. The anchor's own wall-clock position is written once, as `Manifest::anchor_wall`.
- **T2 — re-anchor on restore.** An `Instant` that must survive as an `Instant` (e.g. `SubagentDetail::started_at` / `last_activity_at`, `src/ui/mod.rs:22`, `:26`) is rebuilt from the wall delta, exactly the sanctioned precedent `Plan::get_plan_start_time()` (`src/manager/phase.rs:223-235`): `instant' = Instant::now() - (wall_now - wall_recorded)`. The approximation (suspend/hibernate and wall-clock steps) SHALL be documented at the call site; it is the same trade-off the plan-start fallback already makes.
- **T3 — durations are numbers, never differences of two re-anchored instants.** `worked_secs: f64` (and `SubagentDetail::worked_duration` on restore) SHALL be taken from the persisted number. A duration computed as `re_anchored(a) - re_anchored(b)` is prohibited: two independently anchored instants have no guaranteed relationship.
- **T4 — ordering only.** `saved_elapsed_secs` / `TimePoint::elapsed_secs` are monotonic **within one process** and SHALL be used to order members and detect reordering. They SHALL NOT be compared across processes, and SHALL NOT be used to reconstruct an `Instant`.
- A `TimePoint` whose `wall` does not parse, or whose `elapsed_secs` is negative/NaN/inf, is a structural violation ⇒ `Refused` (D6). No defaulting to "now".

### 5.7 Identity floors: how the rebuilt `WORKERS` table keeps identical ids and counters

Restore SHALL perform, **in this order**, before any restored worker is visible to the rest of the system:

1. **Raise the statics to the floors** (never lower, never assign):
   - `WORKERS.WORKER_ID_SEQ` ← `fetch_max(identity_floors.worker_id_seq_floor)` (`src/orchestrator/workers.rs:130`);
   - `WORKERS.COMPLETED_SEQ` ← `fetch_max(identity_floors.completed_seq_floor)` (`:112`);
   - `notice.NEXT_NOTICE_ID` ← `fetch_max(identity_floors.notice_id_floor)` (`src/orchestrator/notice.rs:34`).
   These three statics are `LazyLock<AtomicU64>` / `AtomicU64` and are process-global, so raising them is legal from any thread and requires no lock. Without step 1 the next worker registration re-draws ids `0,1,2,…` already embedded in restored keys, `claim_worker_slot` then disambiguates them with `#N`, and the UI shows different worker identities after the restart than before it.
   - **Floor arithmetic (Amendment A-3, below §6.6).** Each floor is the **first UNISSUED** value of its counter — every value strictly below it has already been handed out, which is exactly what `worker_id_seq_floor()`/`completed_seq_floor()` report. `fetch_max(floor)` therefore leaves the counter sitting *at* the floor: the **first** post-restore draw **equals** the floor, and only the **second** draw is strictly above it. Treating the floor as "a value that was already spent" (expecting the first draw to exceed it) is off by one.
2. **Insert under the stored key.** Write the constructed `WorkerState` into `WORKERS` with `WORKERS.insert(worker_key, state)` (or an equivalent `session/`-owned installer), keeping `completed_seq` verbatim. No `claim_worker_slot`, no re-minting, no UUID fallback.
3. **Respect the completed ring.** The live map holds at most `MAX_RECENT_COMPLETED = 10` completed entries (`:103`, eviction `:172-190`). Re-insertion SHALL evict oldest-first by `completed_seq` past the cap, and the count SHALL be reported as `RepairReport::completed_dropped_by_ring_cap`. The bundle MAY legitimately contain more worker files than the live map can hold — the files are the durable record, the map is the working set.
4. **Never hold a `DashMap` guard across a restore step.** The snapshot walker and the installer SHALL collect keys into a `Vec` first, following the codebase's own rule (`:171-190`, and the "no panic inside `Drop`" reasoning at `:196-207`) — relevant because a restore may run on a path where a panic would abort the process.
5. **Re-arm the missing capability, loudly.** For each `inProgress` worker: mint a fresh `CancellationToken` for the *new* execution if (and only if) t-006 decides to re-launch it, and emit `NonRestorableKind::CancellationToken`, `JoinHandle`, `DropGuard`, and (when `dangling.dispatched`) `PtyProcess`. Emit `PendingNotice` for any notice that was queued but undelivered. These notices are the difference between "the same in-flight subagents" and "an honest approximation presented as exact".

### 5.8 Per-worker invariants (the checklist §9 will turn into tests)

- I-W1 Restored `worker_key` set == persisted `worker_key` set (modulo the completed-ring cap, reported).
- I-W2 Every restored `worker_id_seq_value` < `identity_floors.worker_id_seq_floor`, and no id below the floor is re-issued after restore.
- I-W3 Restored completed entries keep their relative order by `completed_seq`, and every new completion in the new process draws a strictly larger value.
- I-W4 `history` round-trips byte-exactly through `MessageRecord` (including `content: null` vs `""`, `reasoning_content`, and verbatim `arguments`), and `history_source == LiveMirror` is required before "full history restored" may be claimed.
- I-W5 Restored `status` ⇔ `completed_seq` consistency: `inProgress` ⇔ `completed_seq == None`; `status_label` is byte-identical to the pre-crash label.
- I-W6 A single corrupt worker file yields exactly one `Unrecoverable` worker, `PartiallyRestored`, and a loud report; a second corrupt worker file yields `Refused` with the bundle bytes unchanged.
- I-W7 *(Amendment A-2, below §6.6)* A restored worker whose `status_label` is not one of the recognized labels keeps the label verbatim with `status == Unknown` (§5.3) and is reported by a `warnings` sentence naming the key and the label. It is **not** counted as an interrupted worker: the interrupted set is exactly the `inProgress` projections, and an `Unknown` projection is an open delegation whose interruption claim cannot honestly be made.

---

<!-- Sections 6–10 (UI section field spec, write/commit protocol + fingerprint + corruption decision tree, on-load normalization + `RepairReport` semantics, exactness acceptance criteria, non-restorable inventory) are appended by t-019. -->
---

## 6. UI section field spec (`ui.json`)

§3.6 fixed the two fields D4 pins (`chat_messages`, `status_line`) and explicitly deferred the rest of the struct to this section. The block below **completes that same struct** — it is not a second type. Every field is declared in `session::format` (D1) with `#[serde(deny_unknown_fields)]` on the struct and every nested struct, exactly as §3.1 requires.

Gap numbers in the tables are the numbered gaps in `docs/research_persist_ui.md` §"Gaps that break 'exact text in all boxes'" (1–9). "Restores" means the value is assigned to live state on load; "witness" means the value is persisted **only** to detect drift and is never adopted silently (the `max_context_tokens` pattern of R-PIN-6).

### 6.1 `UiSection` — the complete field set

```rust
/// `ui.json`. Two authorities, exactly one of them for display (D4):
/// `chat_messages` for the chat pane, `subagent_panels` for the subagent panes.
/// `journal` exists for steering correlation only.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    // ---- display authority (fixed in §3.6, unchanged) ----
    pub chat_messages: Vec<String>,
    pub status_line: String,

    // ---- subagent panes (display authority for those panes, §6.3) ----
    pub subagent_panels: Vec<SubagentPanel>,

    // ---- semantic journal mirror (§6.4) ----
    pub journal: Vec<UiJournalRecord>,

    // ---- composer (gap 5) ----
    pub composer: ComposerState,

    // ---- view state: selection + scroll (gap 6) ----
    pub view: ViewState,

    // ---- budget counters (witness only, §6.5) ----
    pub budgets: BudgetWitness,
}
```

| Field | Type | Restores / witness | Live source (verified) | Gap closed |
|---|---|---|---|---|
| `chat_messages` | `Vec<String>` | **Restores**, assigned wholesale to `TuiRenderer.messages` | `src/ui/tui/mod.rs:43` | 1, 2 (and closes the D4 replay trap: the pane prefixes `"User: "` `events.rs:787`, `"[Tool Result] "` `tui/mod.rs:618`, `"Marmennill: "` `events.rs:821+`, `"[CLI] …"` locals are already in the bytes) |
| `status_line` | `String` | **Restores** (else the bar resets to `"Ready"`, `tui/mod.rs:188`, `:897`, `:1030`) | `src/ui/tui/mod.rs:68` | 8 (the numeric part of the bar cannot match after restart — see §6.5) |
| `subagent_panels` | `Vec<SubagentPanel>` | **Restores** (assigned wholesale; the live list boots empty at `session.rs:319`) | `TuiRenderer.subagents: Vec<SubagentDetail>`, `src/ui/tui/mod.rs:83`; model `src/ui/mod.rs:19-37` | 3, 4 |
| `journal` | `Vec<UiJournalRecord>` | **Restores** into the in-memory `UiTranscript` — never rendered | `UiTranscript::records()` (`src/ui/transcript.rs:44-71`); consumer `extract_steering_history` (`:113`) | 1, 9 (durability of the journal that D2 deletes the file for) |
| `composer` | `ComposerState` | **Restores** | `input_text` `tui/mod.rs:53`, `cursor` `:56`, `textarea` `:51`, `history` `:91`, `history_index` `:93`, `input_draft` `:95` | 5 |
| `view` | `ViewState` | **Restores**, then re-clamped by the live clamp (`src/ui/tui/events.rs:466-480`) | `chat_scroll` `:72`, `plan_scroll` `:74`, `subagent_scroll` `:87`, `subagent_list_scroll` `:89`, `selected_subagent_idx` `:85`, `focused_panel` `:70`, `show_thought` `:80`, `show_subagent_panel` `:82`, auto-scroll flags `:124`, `:132`, `:134` | 6 |
| `budgets` | `BudgetWitness` | **Witness only** — compared, never adopted | `thinking_budgets` `tui/mod.rs:159`, `default_thinking_budget` `:161`, set from `Config` by `set_thinking_budgets` (`:256-275`, called `session.rs:267`) | 7 (partially: the *numbers* survive; the `Config` authority does not move) |

### 6.2 Pane-text rules (normative)

- **U1 — one formatter.** Restore SHALL assign strings; it SHALL NOT call any renderer to re-produce them. There is no `UiRecord → chat line` function in the crate today (`docs/research_persist_ui.md` gap 2), and adding one for the restore path is prohibited (D4: byte equality by two independent formatters is not equality).
- **U2 — pane strings are opaque.** No trim, no re-wrap, no line-splitting on `\n` (a single `messages` entry may contain embedded newlines, `tui/mod.rs:42-43`), no Unicode normalization, no ANSI stripping. Empty entries and duplicate entries are **legal** and SHALL survive (the live pane can contain both).
- **U3 — order is content.** The index in `chat_messages` is part of the data; re-ordering is a different pane.
- **U4 — length is checked, not fixed.** `chat_messages.len()` is recorded implicitly by `byte_len`/`fingerprint` (§7) and reported as `RepairReport::ui_lines_restored` (§3.10). A truncated tail is a `fingerprint`/`byte_len` mismatch ⇒ `Refused`, never a "restore what parsed".
- **U5 — no derived state.** `dirty`, `frame_counter` (`tui/mod.rs:66`), every layout cache (`chat_width`/`chat_height`/`subagent_width`/`subagent_height`/`plan_width`/`plan_height`, `cached_message_lines`, `cached_rendered_messages`, `tui/mod.rs:100-122`), `plan_max_scroll` (`:133`) and `active_plan_task` (`:135`) SHALL NOT be persisted or restored: they are recomputed per frame from the new terminal, and persisting them would restore a geometry the new terminal does not have (see §10.5).

### 6.3 `SubagentPanel` — mirror of `SubagentDetail`

```rust
/// Durable projection of one `SubagentDetail` (`src/ui/mod.rs:19-37`), the model
/// behind the subagent list + detail panes. Field-for-field, with the two
/// `Instant`s and the `Duration` replaced by the D5 projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentPanel {
    /// The specialist role id (`coder`, `researcher`, …) — the join key the live
    /// merge uses (`tui/mod.rs:962`: `find(|s| s.name == incoming.name)`).
    pub name: String,
    pub task_id: Option<String>,
    /// Never rendered (`docs/research_persist_ui.md`: "never rendered, but part of
    /// the model"), but required to re-advertise the same subagent, so it is
    /// persisted verbatim.
    pub prompt: String,
    /// Pane text (rendered by `tui/render.rs` for logs/thinking/content).
    pub logs: Vec<String>,
    pub thinking: String,
    pub content: String,
    pub is_active: bool,
    pub context_tokens: usize,
    /// `worked_duration: Duration` as an explicit number of seconds (D5 T3 — never
    /// a difference of two re-anchored instants).
    pub worked_duration_secs: f64,
    /// `started_at: Option<Instant>` / `last_activity_at: Option<Instant>` (§3.9).
    pub started_at: Option<TimePoint>,
    pub last_activity_at: Option<TimePoint>,
}
```

- `logs` / `thinking` / `content` are the **only** recoverable copy of the subagent panes: no `UiRecord` variant exists for them, and `SubagentMessage`/`SubagentThinking` events never reach the journal (`docs/research_persist_ui.md` gap 3). They are therefore display authority for those panes, with U1–U3 applying unchanged.
- `is_active` SHALL be restored **as it was**, and a restored `true` SHALL additionally emit `NonRestorableNotice { kind: JoinHandle, subject: <name> }`: the pane says *Active* because that is what the screen said, while the task that made it active no longer exists (§10.3). Keeping the byte and reporting the loss is the D5 shape — silently flipping it to `false` would change the pane.
- `thinking`/`content` re-enter the live merge at `tui/mod.rs:957-1041`, which keys on `name` and preserves locally streamed chunks; restore SHALL assign the vector **before** the first `set_subagents` call so the merge starts from the restored bytes instead of an empty list.
- Panel order SHALL be preserved (`selected_subagent_idx`, `view.selected_subagent_idx` indexes into this vector; re-ordering would move the user's selection).
- Cross-check with §5: a `SubagentPanel` whose `name`+`task_id` matches no `WorkerSection` is **not** a refusal — the pane and the worker registry are different owners and can legitimately disagree about a worker already evicted from the ≤10 completed ring (`src/orchestrator/workers.rs:103`). It SHALL produce a `warnings` entry naming the panel, and the pane SHALL still be restored byte-exactly.

### 6.4 `journal` — the mirrored `UiRecord` set (steering only)

```rust
/// Persistence mirror of `UiRecord` (`src/ui/transcript.rs:14-40`) — the same 8
/// variants, the same payload types, the same `#[serde(tag = "type")]` layout.
/// Declared in `session::format` (D1) because `UiTranscript` MUST NOT become a
/// persistence type again (D2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum UiJournalRecord {
    User { text: String },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    SteerResponse { text: String },
    ToolCall { display: String },
    ToolResult { display: String },
    TaskCompleted { task_id: String },
    TaskFailed {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Status { text: String },
}
```

- **Sole purpose:** to rebuild the in-memory `UiTranscript` so `extract_steering_history()` (`src/ui/transcript.rs:113`) returns the pre-crash `(prompt, response)` pairs. Pendingness stays delegated to `orchestrator::steer::steering_entry_is_pending` — the durable section carries **no** pending flag, because the queues that decide it are volatile (§10.4).
- **Never display authority.** If a `UiJournalRecord::User { text }` and a `chat_messages` entry disagree (the pane carries the `"User: "` prefix and the journal does not — gap 2/4), `chat_messages` wins for the screen and the journal wins for steering. Neither is derived from the other, at capture or at load (D4).
- The 8 variants are the whole set; a 9th variant in a bundle is an **unknown-field/unknown-variant violation** ⇒ `Refused { code: Json }` (D6), not "skip the record I do not understand".
- Records SHALL keep their append order and their count. `UiRecord::Assistant` stores `content: Option` with `skip_serializing_if` (`transcript.rs:19-22`), which is fine here — the journal is never claimed to be byte-exact history, only steering material. The `None`/`""` discipline of §3.7 applies to `MessageRecord` (manager/worker transcripts), not to this mirror; conflating them is prohibited (§2.1's trap does not apply because nothing sends this type to a provider).

### 6.5 Composer, view state, budgets

```rust
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposerState {
    /// Half-typed message, verbatim, embedded newlines included (gap 5: a
    /// half-typed message currently vanishes).
    pub input_text: String,
    /// Byte offset of the caret (same unit as `TuiRenderer.cursor`,
    /// `tui/mod.rs:53-56`: "always on a char boundary").
    pub cursor: usize,
    /// Draft saved when history navigation began (`tui/mod.rs:95`).
    pub input_draft: String,
    /// Command history, oldest first (`tui/mod.rs:91`).
    pub history: Vec<String>,
    /// `None` = not navigating (`tui/mod.rs:93`).
    pub history_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewState {
    pub chat_scroll: u16,
    pub plan_scroll: u16,
    pub subagent_scroll: u16,
    pub subagent_list_scroll: u16,
    pub selected_subagent_idx: usize,
    /// `"chat" | "plan" | "subagents"` — mirrors `FocusedPanel`
    /// (`src/ui/tui/mod.rs:34-38`); a value outside that set is a refusal.
    pub focused_panel: String,
    pub show_thought: bool,
    pub show_subagent_panel: bool,
    pub chat_auto_scroll: bool,
    pub subagent_autoscroll: bool,
    pub plan_auto_scroll: bool,
}

/// Witness-only: the budgets are owned by `Config` (`tui/mod.rs:256-275`, applied
/// at `session.rs:267`), so a persisted difference is reported, never adopted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetWitness {
    /// Sorted map so the committed bytes are deterministic across runs.
    pub thinking_budgets: std::collections::BTreeMap<String, usize>,
    pub default_thinking_budget: usize,
}
```

- `composer.input_text` is restored by assigning `input_text` + `cursor` and letting the existing sync path rebuild the widget: `ensure_textarea_in_sync` (`src/ui/tui/events.rs:350-360`) constructs the `TextArea` from `input_text.split('\n')`, and `sanitize_cursor` (`:362-372`) clamps the caret to a char boundary. Restore SHALL reuse those two helpers rather than hand-building a `TextArea` (D1 seam rule: the renderer owns its widget, `session/` owns the bytes).
- **Selection.** `cursor` is the caret (byte offset, U-chk below); the `TextArea`'s internal undo/redo stack and any in-widget selection anchors are **not** restorable (ratatui-textarea internals, `docs/research_persist_ui.md` gap 5 row "Undo/redo stack") ⇒ `NonRestorableNotice { kind: Other }`. The pane-visible consequence is "the caret is where it was, the undo history is gone".
- **Order of application matters** (gap 6 + `docs/research_persist_ui.md` implication 6): assign `chat_messages` first, then the `*_auto_scroll` flags, then the scroll offsets. The live clamp at `events.rs:466-480` then re-clamps every offset against the freshly restored content, which is the *desired* behaviour: a restored offset that exceeds the new content is clamped by existing code instead of being validated by new code. A clamp that moves a restored offset SHALL be recorded in `RepairReport::warnings`.
- **Budget drift is a warning, not a refusal:** like R-PIN-6, `thinking_budgets`/`default_thinking_budget` are re-applied from `Config`; if the persisted witness differs, the loader SHALL report both values in `warnings` and keep the `Config` values. The per-agent maps `subagent_turn_thinking` / `subagent_is_thinking` (`tui/mod.rs:163`, `:165`) are per-turn streaming state and SHALL NOT be persisted.
- **U-chk (structural preconditions for `ui.json`, checked before any assignment):** `cursor <= input_text.len()` **and** `input_text.is_char_boundary(cursor)`; `focused_panel` ∈ {`"chat"`,`"plan"`,`"subagents"`}; `history_index` is `None` or `< history.len()`; `selected_subagent_idx < subagent_panels.len()` (when `subagent_panels` is empty it MUST be `0`); every `TimePoint` satisfies §5.6's last rule. Any violation ⇒ `Refused { code: HeaderManifestDisagreement, member: "ui.json" }`. These are preconditions, not repairs: the loader never "fixes" a caret or a selection.
- **Not persisted, by decision:** `confirm_abort` / `abort_armed_at` (`tui/mod.rs:58`, `:60`) and `InputState` (`ui/mod.rs:75-80`) — a 3-second armed window has no meaning after a restart; `plan_content` (`tui/mod.rs:49`) and `show_plan_panel`/`had_active_plan` — re-read from `.marmel/execution_plan.md` at `init` (`tui/mod.rs:417-424`), so the plan file remains the single authority; `active_agent` (`tui/mod.rs:142`) — re-derived from the restored worker/manager state at boot; `session_start` / `last_render` / `waiting_for_token_since` — animation clocks.

### 6.6 UI-section invariants

- I-U1 `chat_messages` round-trips byte-exactly, element by element, including empty and duplicated entries.
- I-U2 No entry of `chat_messages` is ever produced by a renderer call on the restore path (testable by asserting the pane buffer equals the persisted vector *before* the first draw).
- I-U3 `subagent_panels` count, order and every pane string (`logs`/`thinking`/`content`) match; `worked_duration_secs` is taken from the number, never recomputed from `started_at`/`last_activity_at` (D5 T3).
- I-U4 The restored `UiTranscript` yields exactly the same `extract_steering_history()` pairs as the pre-crash journal, and no pane line is derived from it.
- I-U5 `composer.input_text` and `composer.cursor` match; the caret lands on the same char boundary.
- I-U6 Every restored scroll offset is either byte-identical or re-clamped by the existing clamp with a recorded `warnings` entry.

---

## Amendments — §6 field set (t-020) and §5 identity / status clarifications

This block records **additive amendments and wording corrections**. Nothing here relaxes a MUST or a SHALL above: where an amendment corrects a *description* (what a field or a counter actually does), it says so, and the normative rule keeps its force.

**A-1 — `ui.json` carries fourteen further fields, added later by task t-020, with no `SCHEMA_VERSION` bump.** `session::format::ui::UiSection` now also holds `plan_content`, `had_active_plan`, `active_plan_task`, `plan_start`, `plan_completed`, `current_thought`, `current_content`, `active_agent`, `steer_sentence_buffer`, `tokens_in`, `tokens_out`, `orchestrator_context_tokens`, `abort_armed`, `abort_armed_at`. `ViewState` additionally holds `show_plan_panel`, and `SubagentPanel` holds `turn_thinking` / `is_thinking`. Every one is declared `#[serde(default)]`, which is precisely why §3.1's `SCHEMA_VERSION` stays at its published value: a defaulted field cannot make an older bundle unparseable, so the version gate is not the mechanism carrying them, and `#[serde(deny_unknown_fields)]` still refuses a bundle written by a newer release. Two consequences for §6.5's wording:

- The bullet "**Not persisted, by decision**" is **superseded for the fields this amendment persists**: `plan_content`, `had_active_plan`, `show_plan_panel`, `active_plan_task`, the plan display timers (`plan_start` / `plan_completed`), the streaming partials (`current_thought` / `current_content`), `active_agent`, `steer_sentence_buffer` and the abort-arming record ARE now persisted. What did not change is how they are applied: the token counters are witnesses and are never adopted; `abort_armed` / `abort_armed_at` are recorded while the live renderer is left unarmed and the drop is announced; and an empty defaulted plan group is read as "this bundle predates the field", never as "the pane was blank", so the disk-backed plan pane keeps its `init` reload path and `.marmel/execution_plan.md` stays the authority for the checkboxes.
- `InputState`, `session_start` / `last_render` / `waiting_for_token_since` and the per-frame layout caches remain unpersisted, exactly as §6.5 and §10.5 decided.

**A-2 — an unrecognized `status_label` is reported as a warning, not as an interruption.** §5.3 already fixes the bytes (`status == Unknown`, label kept verbatim; AC-W3 asserts the bytes). Which report names it: a `warnings` sentence carrying the worker key and the label. The interrupted-projection set (`interrupted_keys`) does **not** contain that key — the honest interrupted claim belongs to the `inProgress` projections only. Recorded as §5.8 I-W7.

**A-3 — the identity floors are first-UNISSUED values.** Full statement at §5.7 step 1: after `fetch_max(floor)` the **first** post-restore draw **equals** the floor, and only the second draw is strictly above it. §9.4 AC-I1's `>` must be read with that correction (assert the first draw against `>=` and the later ones against `>`); the remainder of AC-I1 — every restored `worker_id_seq_value < floor`, no `#N` re-disambiguation of a restored key — is unaffected.

---

## 7. Write/commit protocol (`session::commit`) — t-019

D3 fixed the shape (split-file bundle, manifest-last = the atomicity point). This section fixes the order, the integrity function, the stale-file rules, and the decision tree the loader walks. Ownership: everything here is `session::commit` / `session::restore` (D1). There is exactly **one** writer routine in the crate.

### 7.1 Member write order (normative)

```text
C1. compute payloads            manager.json, ui.json, every workers/<key>.json
                                serialize ONCE each -> owned Vec<u8>
C2. diff against the last committed bytes (in-memory cache of {rel_path, byte_len, fingerprint});
    a member whose bytes are identical to the committed member is NOT rewritten
C3. write the changed members, in this order ONLY:
        (a) workers/<key>.json   ascending rel_path
        (b) ui.json
        (c) manager.json
C4. manifest.json  <- LAST, nothing after it. This rename IS the commit.
C5. post-commit sweep (best effort, never load-bearing): delete files under
    workers/ that the NEW manifest does not list, and stale ".{stem}.tmp.*"
    staging names that are not this process's.
```

- **C4 is the atomicity point.** A reader that finds a `manifest.json` that parses sees a set of members that each completed an atomic rename *before* it (§1 D3). If the process dies between C3 and C4 there is **no committed bundle for that instant**: the previous manifest is still on disk and the rewritten members no longer match it ⇒ the next load refuses (§7.5). That is the accepted price of fail-closed (D6), and C2 exists to shrink it: in the common case one or two members change per commit, so the exposed window is a single rename.
- **C3's order is deliberate.** The worker mirrors are the largest payload and the only members whose corruption is tolerable (D6), so they go first: an interruption there costs at most the partial case the design already allows. `manager.json` goes last among members because it is the transcript the *next request* is built from and the one the pin rules verify (§4) — the fewer member renames outstanding behind it, the smaller the set that can disagree with the manifest it is about to get. `ui.json` sits between them so that a pane-text write can never be skipped *after* the transcript it renders was already replaced.
- **Never write a member the manifest will not list**, and never list a member that was not written. A `manifest.json` naming a path that is absent on disk is a `RefusalCode::MissingMember`.
- **Every member write goes through `harness::fs::write_file_atomic`** (`src/harness/fs.rs:475-503`): unique `O_EXCL` staging name in the same directory (`.{stem}.tmp.{pid}.{nanos}.{seq:08}`, `:395`), `write_all` + `flush` (`:484-489`), permission preservation (`:492`), same-directory `rename` (`:494`), and `StagedTemp::drop` unlink on every failure path (`:482`, `:501`). A session writer SHALL NOT call `std::fs::write` for a bundle member — that is exactly how today's journals get truncated (`context.rs:993`, `transcript.rs:96-97`).
- **Honest durability limit (stated, not hidden).** `write_file_atomic` flushes the file but does **not** `fsync` the file or the parent directory (verified: no `sync_all`/`sync_data` anywhere in `src/harness/fs.rs`). Rename gives *atomicity* (a reader sees old-or-new) but not *durability* against power loss. The consequence is bounded by the design: a power cut can leave the new `manager.json` lost while a newer manifest survived — which is a `byte_len`/`fingerprint` mismatch ⇒ `Refused`, never a half-restored session. A torn member is not reachable through this protocol.
- **Commit serialization.** `session::commit` SHALL serialize committers in-process (a `session`-owned mutex around C1–C5) — the real case is the panic-hook commit (`ProcessEnding::PanicHook`, `src/main.rs:277`) racing an in-loop commit. It SHALL NOT hold any `DashMap` guard across a write (§5.7 step 4). Note the anchor in §2.5 ("a bundle writer SHALL take the per-path lock for `manifest.json`") cannot be used as written: `harness::fs::replace_path_lock` (`src/harness/fs.rs:342`) is **private** to that module. Implementing it means either a visibility change in `harness::fs` or a `session`-owned lock (this section prescribes the latter). See §11.
- **Commit results are never discarded.** The 22 `let _ = ctx.save_transcript(..)` / `let _ = ui_transcript.save(..)` sites (D2) are replaced by commit calls whose `Result` feeds the capture-trigger bookkeeping. A failed commit SHALL raise a `warnings` entry and a `tracing::warn!` with the failing `rel_path` and the `ToolError` text.
- **Panic-hook path.** The hook is pure-sync (no `block_on`, no `into_inner` on poisoned locks other than the sanctioned pattern, no panic permitted). C1–C4 use only `serde_json::to_vec_pretty`, `Vec<u8>`, and `write_file_atomic`, all synchronous. The hook SHALL write `ending: PanicHook` and SHALL NOT attempt the C5 sweep.

### 7.2 The fingerprint function (FNV-1a 64)

```rust
/// `session::format` — the only fingerprint in the crate (D1).
/// FNV-1a, 64-bit: the same constants already used by the crate for
/// deterministic shard selection (`src/harness/fs.rs:352-359`).
pub fn fingerprint(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325; // FNV offset basis
    for byte in bytes {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3); // FNV prime
    }
    hash
}
```

- **F1 — over exact bytes.** `fingerprint(&bytes)` and `byte_len = bytes.len() as u64` SHALL be computed from the **same owned buffer** that is handed to `write_file_atomic`. Recomputing either by re-serializing the value is prohibited: the loader then compares a value against itself and the check proves nothing.
- **F2 — no new dependency.** `fnv` appears in `Cargo.lock` (line 754) only as a transitive dependency and is not in `Cargo.toml`; the crate SHALL hand-roll the 6 lines above (the constants are already in the codebase at `fs.rs:353-356`) rather than add a dependency.
- **F3 — not a reuse of `hash_path`.** `harness::fs::hash_path` is private, takes a `Path`, returns `usize`; it is a shard selector, not an integrity witness.
- **F4 — reported, not silently compared.** Every comparison prints both sides in hex (`fingerprint 0x1b3d… != 0x9a77…`) and both lengths in bytes, so a reader can reproduce the number by hand against a pretty-printed member file (§3 preamble: the format is debuggable).
- **F5 — coverage.** The fingerprint covers the whole member file, header included. There is no partial/canonicalized fingerprint, no per-record digest, and no hash of the *parsed* value.

### 7.3 Verify-before-parse order (loader)

```text
L0. bundle dir absent, or manifest.json absent        -> NoBundle (+ legacy-journal warning, D2)
L1. read manifest.json bytes                          -> IO error => Refused(Io)
L2. parse manifest.json                               -> parse error => Refused(Json)
L3. format_version == FORMAT_VERSION                  -> else Refused(FormatVersion)
    schema_version == SCHEMA_VERSION                  -> else Refused(SchemaVersion)
    (deny_unknown_fields violation                     -> Refused(UnknownField))
L4. for each member in manifest.members[] (order irrelevant):
      stat + read the WHOLE file                       -> missing => Refused(MissingMember)
      byte_len == member.byte_len                      -> else Refused(ByteLen)
      fingerprint(bytes) == member.fingerprint         -> else Refused(Fingerprint)
L5. parse the member; check SectionHeader against the manifest and the Manifest:
      format/schema versions, session_id, kind, saved_wall, saved_elapsed_secs,
      member_count/member_index                       -> any disagreement => Refused(HeaderManifestDisagreement)
      worker members: decoded rel_path key == WorkerSection.worker_key  (§5.2)
L6. ManagerSection pin verification (§4 R-PIN-5)      -> Refused(PinViolation)
L7. structural preconditions of each section          -> Refused(HeaderManifestDisagreement)
    (TimePoint sanity §5.6, UiSection U-chk §6.5, IdentityFloors sanity §5.7)
L8. normalization passes (§8), exactly twice          -> unstable => Refused(NormalizationUnstable)
L9. install state. Nothing before L9 touched live state; nothing at all touched disk.
```

- Integrity fields are verified **before** parsing (D3: the manifest is the authority, a section never trusts its own header). A member that parses fine but whose length or fingerprint disagrees is still a refusal — "it parsed" is not evidence of completeness.

### 7.4 Stale members and orphan files

- **Orphan = on disk, not listed in the manifest.** Ignored, never merged, never partially read (§1 D3). The loader SHALL count them and put the count and the names in `RepairReport::warnings` — an orphan is evidence of an interrupted overwrite, and "ignored" must not mean "invisible".
- **Stale member = listed, older than expected.** Impossible to miss: the manifest is the only member set, so an older `manager.json` under a newer manifest is a `byte_len`/`fingerprint` mismatch, not a stale file.
- **A member that survives the thing it describes.** Worker files legitimately outlive the live ring: the bundle can hold more worker files than `WORKERS` holds (`MAX_RECENT_COMPLETED = 10`, `src/orchestrator/workers.rs:103`), and §5.7 step 3 reports how many completed entries were dropped. Those extra files are **not** orphans — they are listed in the manifest — and restore SHALL NOT delete them.
- **`session::restore` is read-only with respect to disk.** No rewrite, no in-place repair, no rename, no delete, no `.bak`. `Refused` leaves every byte of the bundle for post-mortem inspection (D6). The C5 sweep belongs to the committer only.
- **Leftover staging names** (`.{stem}.tmp.*` in the bundle directory, reachable only when a process is killed between the `O_EXCL` create and the rename) are never members: the loader SHALL ignore every bundle-directory entry whose name starts with `.`, and SHALL NOT report those as orphans (they are noise, not state).
- **Legacy journals** (`.marmel/.session_transcript.json`, `.marmel/.ui_transcript.json`) are never merged, upgraded, or deleted: they produce the `NoBundle` + "legacy journals found, not restorable" warning naming both paths (D2), and they are not counted as orphans of this bundle.

### 7.5 Fail-closed decision tree

| Condition found | Verdict | Bundle bytes | Loud output |
|---|---|---|---|
| no `manifest.json` (`manager.json`/`ui.json`/`workers/*` may exist) | `NoBundle` | untouched | warning naming the directory; orphan members counted; legacy-journal warning if applicable |
| `manifest.json` present but unparsable | `Refused { Json, member: "manifest.json" }` | untouched | refusal reason + the `serde_json` error text with line/column |
| `format_version` / `schema_version` not implemented | `Refused { FormatVersion \| SchemaVersion }` | untouched | both numbers: file's and this binary's |
| unknown field anywhere (`deny_unknown_fields`) | `Refused { UnknownField, member }` | untouched | the offending field name and member |
| a listed member file is missing | `Refused { MissingMember, member }` | untouched | `rel_path` + the manifest's `byte_len`/`fingerprint` |
| member `byte_len` mismatch | `Refused { ByteLen, member }` | untouched | expected/actual bytes |
| member `fingerprint` mismatch | `Refused { Fingerprint, member }` | untouched | expected/actual hex |
| member parses but is a **truncated tail** (array/object ends mid-value, trailing `}` missing) | always arrives as `Json`, `ByteLen` or `Fingerprint` — a torn member is not reachable through C3/C4, so any of the three means "the bytes are not the committed bytes" | untouched | all three numbers, computed by hand if the reader wants |
| header ↔ manifest disagreement (session id, kind, saved_* , member_count/index) | `Refused { HeaderManifestDisagreement, member }` | untouched | both sides of each field |
| a section's structural preconditions fail (`TimePoint` NaN/negative §5.6, `UiSection` U-chk §6.5, `messages.len() < 2` §4) | `Refused { HeaderManifestDisagreement \| PinViolation, member }` | untouched | field path and value |
| pin violation (any of R-PIN-5 steps 1-5) | `Refused { PinViolation, member: "manager.json" }` | untouched | which pin, both lengths, the recomputed `pinned_prefix_end` |
| normalization not idempotent (§8) | `Refused { NormalizationUnstable, member }` | untouched | every rule that applied on pass 1 and on pass 2, with before/after counts |
| I/O error reading any member | `Refused { Io, member }` | untouched | the OS error |
| **exactly one** `workers/<key>.json` fails L4/L5 (unparsable, wrong length, wrong fingerprint, header disagreement, key/escape disagreement, **or absent**) **and** every other member verifies, **and** the section's `worker_key` is recoverable from the manifest `rel_path` escape (§5.2) | `PartiallyRestored { …, unrecoverable_workers: [key] }`; that worker is inserted with `WorkerStatus::Unrecoverable` and `unrecoverable_reason` naming which check failed | untouched | `tracing::warn!` + UI status line + journaled `UiRecord::Status` + `NonRestorableNotice { kind: WorkerHistory, subject: key }`; `RepairReport.workers_unrecoverable` |
| two or more worker members fail that check, **or** `manager.json`/`ui.json` fails it, **or** manifest/section disagreement | `Refused` (D6's "any other partial") | untouched | every failing member, listed |
| `ending == InProgress` | **not** a refusal | untouched | reported as `ProcessEnding::HardKillInferred` |

- The `Unrecoverable` tolerance is **narrow on purpose**: one member, worker kind only, everything else verified. Its history is gone (`WorkerHistory` notice, `history_source` meaningless), and its pane identity is kept so the UI still shows that the subagent existed (§3.8). A *missing* worker file is treated the same way — an absent member and an unreadable one produce the same user-facing truth: that one subagent is gone while the rest of the session is real. Any reader who objects must fix D6, not this table.
- **The refusal message is a first-class deliverable.** Each `Refused` SHALL print: the code, the member `rel_path`, the two numbers involved, the bundle path, and the sentence "the bundle was not modified — it is still on disk unchanged". A refusal that hides which byte disagreed fails D6 even if it refuses correctly.

---

## 8. On-load normalization — the healing passes and their report

A restored transcript MUST be sendable to a provider on the first request after the restart. A crash almost never leaves one that is: `src/ui/session.rs:689-690` appends the assistant turn (with its `tool_calls`) and writes the transcript **before** the round's dispatch, and the boundary repair at `src/ui/session.rs:1788-1795` (`ensure_pairing_at_boundary`, helper at `:2153-2174`) runs only after a completed round. Ctrl-C / `kill` / a crash therefore lands in the one window the live loop never repairs.

**Ownership rule (D1):** `session/` SHALL NOT implement a single pairing rule. Every rule is the existing `manager::context` grammar, called through the engine seam. A second implementation of pruning, grouping or placeholder text is prohibited — it is how the capture side and the load side diverge.

### 8.1 The helpers, reused verbatim

| Pass | Reused function (verified) | What it actually does |
|---|---|---|
| N1 orphan + duplicate pruning | `prune_orphan_tool_messages` (`src/manager/context.rs:339-356`) | drops `Message::Tool` whose `tool_call_id` has no surviving assistant parent |
| N2 regroup + synthesize | `repair_tool_call_pairs` (`src/manager/context.rs:384-434`), called through `ContextEngine::ensure_tool_call_pairs` (`:897-903`) | (1) reuses N1 and additionally drops **duplicate** results for an id, keeping the first (`:385-387`, doc `:364-365`); (2) **moves** every surviving result directly under its parent assistant, in `tool_calls` order (`:402-428`); (3) synthesizes `Message::Tool { tool_call_id, content: ABORTED_TOOL_RESULT }` for every `tool_calls` entry with no result (`:413-422`) |
| placeholder text | `ABORTED_TOOL_RESULT` = `"(aborted)"` (`src/manager/context.rs:327`; `session::format::ABORTED_TOOL_RESULT`, §3.1, pinned equal by a compile-time test) | the only placeholder vocabulary in the crate |
| change detection | `pairing_shape` (`src/manager/context.rs:444-455`): role tag + `tool_call_id`s, content excluded | what `ensure_tool_call_pairs` compares to decide "changed" (`:897-903`) |
| documented idempotence | `context.rs:887-890` — "running it on an already valid transcript is a no-op that returns `false`" | the basis of N5 below |

Because `repair_tool_call_pairs` only prunes, moves and synthesizes — never edits content (`context.rs:441-443`) — it is safe to run over restored bytes, and it is the reason the loader does not need, and MUST NOT have, its own repair.

### 8.2 Pass order (normative, one pass set for manager **and** every worker)

```text
N0  decode                 MessageRecord -> Message field by field (MessageRecord::into_wire, §3.7).
                           Prohibited: any route through serde, ToolCall::new,
                           sanitize_arguments or ensure_valid_json_arguments
                           (wire.rs:88-108, :135-138) — they rewrite arguments.
N1  prune orphans          prune_orphan_tool_messages.
N2  regroup + synthesize   repair_tool_call_pairs, through ContextEngine::ensure_tool_call_pairs.
N3  pin re-verification    re-run §4 R-PIN-5 steps 1-5 on the NORMALIZED vector.
N4  re-encode              Message -> MessageRecord field by field (from_wire). Byte-exactness of
                           content: None vs Some(""), reasoning_content and verbatim arguments is
                           asserted here, not assumed.
N5  stability check        run N1+N2 a second time. ensure_tool_call_pairs MUST return false.
                           Anything else => Refused { NormalizationUnstable, member }.
N6  witness cross-check    compare the recorded DanglingToolCallState (§3.9) against what N1/N2
                           actually did; a disagreement is reported, never reconciled.
```

- **N3 is not redundant with L6 (§7.3).** L6 verifies the pins of the *stored* transcript; N3 verifies the pins of the *installed* transcript. `repair_tool_call_pairs` can in principle move a message inside the pinned window (it never moves `System`/`User` — `context.rs:371-374` — but it can prune an orphaned `Tool` result sitting between the assistant and the next turn, which shifts indices). If a pass changes the recomputed `pinned_prefix_end`, the outcome is `Refused { PinViolation, member }` with `pinned_prefix_end_expected` / `pinned_prefix_end_actual` filled in. There is no "repair moved the window, accept it" branch.
- **`messages[0]` and `messages[1]` are never touched by any pass.** They are `System`/`User`, and the grammar moves and prunes only `Tool` messages, so the invariant holds by construction (`context.rs:371-374`); N3 asserts it anyway (role at `[0]`/`[1]`, lengths equal to `pin_witness`) because REQ-CORE-001/002 degradation is silent.
- **Advisory and checkpoint text is opaque** (R-PIN-4): no pass may trim or reword a message whose content starts with an `INJECTED_ADVISORY_PREFIXES` marker (`context.rs:305-309`). Pruning such a message is legal **only** if it is an orphaned `Tool` result, which an advisory never is.
- **N1 before N2 is not optional in reporting terms.** `repair_tool_call_pairs` calls `prune_orphan_tool_messages` internally (`:387`), so N2 alone would produce the right transcript and the wrong report: the loader runs N1 first so that *which* ids were pruned as orphans is distinguishable from which were dropped as duplicates. A run that only calls N2 cannot fill `RepairReport` honestly, and is therefore a defect even when its output is byte-identical.
- **Worker history is normalized by the same passes** (§5.4), through the same seam, with the same stability rule. A worker whose history is still pair-invalid is `Unrecoverable`/refused as §5.4 and §7.5 dictate — never "restored with a caveat".
- **`Healed` is not re-healed.** If the capture recorded `DanglingToolCallState::Healed { aborted_tool_call_ids }` (§3.9), those placeholders are already in `history`; N2's `pairing_shape` sees a valid transcript and returns `false`. If N2 nevertheless synthesizes a result for an id that `Healed` already lists, that is a N6 disagreement ⇒ `warnings`, and (if it changes the shape) N5 decides.

### 8.3 The two hazards, spelled out

**H1 — dangling `tool_calls` (the common crash shape).** The assistant turn carrying `tool_calls` is persisted at `src/ui/session.rs:689-690` and the process dies before `:1788-1795`. The bundle's `ManagerSection.dangling` reads `Dangling { calls: […dispatched: true|false…] }`. On load:
- N2 synthesizes one `"(aborted)"` result per id, in `tool_calls` order — this is a **documented, intentional loss** of a nonexistent tool output (the option-(a) decision recorded at `context.rs:376-383`), not a data-integrity failure.
- Every synthesized id is reported by name, with its function name and verbatim arguments from `DanglingCall` (`§3.9`), so the user can see which call the model made and never got an answer for.
- `dispatched: true` additionally raises `NonRestorableNotice { kind: PtyProcess }` (§5.4, §10.2): the tool may have run and mutated the workspace while its result never landed. The loader SHALL NOT re-run the call and SHALL NOT assume the side effect happened — both are claims it cannot make.
- The transcript is then valid to send. The model sees its own call answered `"(aborted)"`, which is precisely what the live abort path already produces (`session.rs:1127-1135` breaks the tool round, and the live boundary repair fills the same placeholder).

**H2 — orphan `role:"tool"` results.** A result whose parent assistant turn is gone (truncated tail of a compaction, a rebirth collapse, a hand-edited bundle). N1 drops it and reports each dropped id and its original index. A drop here is loud, never silent (D6): the count is `RepairReport.orphan_tool_results_pruned`, and each id appears in `warnings` as `pruned orphan tool result <tool_call_id> at message index <i>`.
- **Duplicates** (two results for one id) are handled by N2's map keeping the first (`context.rs:389-400`); the loader SHALL report the count as a `warnings` entry because `RepairReport` has no dedicated field for duplicates (§8.4).

### 8.4 `RepairReport` — exact field semantics

`RepairReport` (§3.10) is the loader's only account of what it did. Rules:

| Field | Meaning on every path that ran normalization |
|---|---|
| `pins_verified` | `true` only if N3 passed. `false` never coexists with `Restored` |
| `pinned_prefix_end_expected` / `_actual` | persisted witness vs recomputed from the installed vector. Both `Some` on every restore/refusal that read `manager.json` |
| `orphan_tool_results_pruned` | number of `Tool` messages N1 dropped (manager + workers, summed); each id in `warnings` |
| `aborted_results_synthesized` | number of `"(aborted)"` results N2 added; each id in `warnings` as `synthesized ABORTED_TOOL_RESULT for tool_call_id <id> (function <name>, manager\|worker <key>)` |
| `tool_results_regrouped` | number of `Tool` messages whose index changed (moved under their parent). **Counted against the PRE-normalization (as-decoded) vector**, and the `index_before` / `index_after` pair reported for each moved id is likewise taken from that as-decoded vector — the pair a reader can reproduce from the bytes in the bundle, not an index in the healed vector (Amendment B-1 below) |
| `normalization_passes` | MUST be `2` on any path that ran N0-N6. `0` only on `NoBundle`/refusals that never reached normalization |
| `normalization_stable` | MUST be `true`; `false` ⇒ the outcome is `Refused { NormalizationUnstable }` |
| `manager_messages_before` / `_after` | `before` = count as decoded, `after` = count installed. The difference is the net of pruning and synthesis and is expected to be non-zero after a kill |
| `ui_lines_restored` | `UiSection.chat_messages.len()` as assigned (§6.1) |
| `workers_restored` | worker sections installed by the loader, **excluding** any worker it marked `Unrecoverable` — the field means *restored*, and the tolerated partial's keys belong to `workers_unrecoverable` (Amendment B-2 below). The superseded wording read "present in the rebuilt table" and pointed at §5.8 I-W1 — which describes the restored key **set** (that set does include the tolerated `Unrecoverable` entry), not this count. |
| `workers_unrecoverable` | keys, in bundle order, for §7.5's single tolerated partial |
| `completed_dropped_by_ring_cap` | §5.7 step 3 |
| `notices` | every D5 projection that lost a capability (§3.11, §10) |
| `warnings` | free-text, one sentence each, containing the numbers a reader would compute by hand: duplicate-result counts, per-worker before/after counts (`worker <key>: history <before> -> <after>`), N6 disagreements, orphan member counts (§7.4), clamped scroll offsets (§6.5), budget drift (§6.5), `max_context_tokens` drift (R-PIN-6) |

- **Per-worker detail lives in `warnings`.** §3.10's `RepairReport` has manager-level before/after counts only; §8 keeps that shape (it does not re-open §3) and requires the per-worker numbers as `warnings` sentences. If t-014/t-010 want them structured, that is a field addition to §3.10 — recorded in §11 as a shape gap, not silently assumed here.
- **Loudly, three times, on every restore that changed anything** — the precedent is the live boundary repair (`session.rs:2153-2174`), which already writes a `tracing::warn!`, a `renderer.on_event(Event::Status(…))` and a `UiRecord::Status`. Restore SHALL do the same three for every non-zero `orphan_tool_results_pruned` / `aborted_results_synthesized` / `tool_results_regrouped`, and for every refusal and every `PartiallyRestored`. `let _ =` on any of the three is prohibited (D6).
- **The message names the placeholder.** The live sentence is `tool-call pairing repaired (after tool round): missing tool results synthesized as `(aborted)`` (`session.rs:2167-2169`); the restore sentence SHALL be the same shape with the site spelled `"on session restore"` and the member path added, so a user who saw the live message recognizes the restore message.
- **Nothing is written back.** A report describing repairs does not license rewriting the bundle: N0-N6 are in-memory (D6, §7.3 L9). The next commit writes the normalized vector as the new `manager.json`/`workers/<key>.json` — through the normal commit protocol, at the normal commit trigger, not as part of the restore.
- **Non-idempotence is a verdict, not a retry.** A second pass that changes anything means the input admitted two incompatible readings (the documented contract at `context.rs:887-890` was violated). The loader SHALL NOT loop to a fixed point, SHALL NOT pick one reading, and SHALL NOT persist a normalized copy to "make it stable": `Refused`, bytes untouched, both pass reports printed.

---

## Amendments — §8.4 `RepairReport` field semantics as implemented

Additive corrections to two §8.4 prose rows. No normative sentence above is weakened: the loudness rules (three destinations, no `let _`), the "nothing is written back" rule, and the non-idempotence verdict keep their full force.

**B-1 — `tool_results_regrouped` is counted against the PRE-normalization vector.** The count is the number of `Tool` messages that moved, measured over the **as-decoded** vector (the one decoded from the member file, before N1/N2 touched it), and the `index_before` / `index_after` pair reported per moved id is also measured over that vector — §8.4's own requirement that the report contain "the numbers a reader would compute by hand" is only satisfiable against the bytes in the bundle. A count or index pair taken from the healed vector is not reproducible from the member file and is therefore not the same quantity.

**B-2 — `workers_restored` excludes `Unrecoverable` workers.** The loader's `workers_restored` counts the worker sections it actually restored, so the single tolerated corrupt worker (installed with `WorkerStatus::Unrecoverable`, §7.5 / D6) is **not** in that count; it appears in `workers_unrecoverable` instead, and `PartiallyRestored.unrecoverable_workers` names it again. Concretely: a bundle with two worker members, one of them corrupt, reports `workers_restored == 1` and `workers_unrecoverable == [<that key>]` — witness `tests/test_session_bundle.rs:810` inside `exactly_one_corrupt_worker_restores_everything_else_as_unrecoverable`. Two denominators are in play and only one of them is `workers_restored`: the boot-time worker installer keeps its own `WorkerRestoreReport::installed`, which counts every entry inserted into the live table (the `Unrecoverable` placeholder included) and is the "present in the rebuilt table" figure §5.8 I-W1 describes for the key **set**. Prose and tests that quote `workers_restored` mean the loader's restored count, not that figure.

**B-3 — what "atomic" buys, stated once (cross-referenced from §2.5).** `harness::fs::write_file_atomic` stages, writes, flushes and renames; it performs no `fsync`/`sync_all` on the file or the parent directory. So a bundle member is atomic with respect to **process death** — the reader sees the old bytes or the new bytes, never a mixture — and is **not** durable against a whole-machine power loss. The design's failure model absorbs the difference: a power cut can leave a manifest whose described member bytes are absent, which surfaces as `byte_len`/`fingerprint` ⇒ `Refused` with the bundle untouched (§7.1, §7.3, §7.5), never as a partially healed session. A `--resume` after power loss may therefore refuse; because a refusal modifies nothing, inspecting or repairing the members and retrying is safe.

---

## 9. Exactness acceptance criteria (seed for the future integration test)

Each AC is one assertion a test can make without inventing a formatter. `MessageRecord`, `Manifest`, `ManifestMember`, `UiSection` and `WorkerSection` all derive `PartialEq, Eq` (§3.2, §3.6, §3.7, §3.8), so "exact" means `assert_eq!` on the durable values — never a hand-rolled canonical rendering, which is the failure mode §2.1 (wire.rs row) and D4 exist to prevent.

Test shape for all of them: build live state → `session::commit` → drop the process state → `session::restore` → compare. Where an AC says "element-by-element", it means index-by-index with the index in the assertion message.

### 9.1 Manager transcript

1. **AC-M1** `restored_manager.messages.len() == committed_manager.messages.len()`, and for every `i`: `restored.messages[i] == committed.messages[i]` (compare `MessageRecord`s directly, not their wire forms).
2. **AC-M2 `content: None` vs `Some("")` survive.** Fixture contains an assistant turn with `content: None, reasoning_content: None, tool_calls: [<one call>]` and another with `content: Some(String::new())`. After the round-trip the first is still `Assistant { content: None, … }` and the second is still `Assistant { content: Some(""), … }`. The bundle bytes for the first contain `"content": null` (assert on the serialized member, not only on the value).
3. **AC-M3 `reasoning_content` survives**: `Some(text)` stays `Some(text)`; absent stays absent; an empty string stays `Some("")`.
4. **AC-M4 `tool_calls` survive verbatim**: same count, same order, same `id`, same `type` key (default `"function"` spelled exactly), same `function.name`, and `function.arguments` byte-identical — including a payload that already carries `{"_error":"malformed_arguments",…}` (must not be re-sanitized, `src/types/wire.rs:88-108`, `:135-138`) and including a payload with non-canonical key order or whitespace (must not be re-serialized).
5. **AC-M5 `role:"tool"` results survive** with byte-identical `content` and exact `tool_call_id`; a synthesized `"(aborted)"` result committed by the live boundary repair comes back as a normal result, indistinguishable in content (§2.2, `context.rs:327`).
6. **AC-M6 no re-role, no re-order**: the sequence of role tags is identical to the committed sequence, except for `Tool`-message moves made by §8 N2 — and for those, the test asserts that each moved result sits directly after its own parent assistant and that every non-`Tool` index is unchanged.
7. **AC-M7 legacy advisory text is untouched**: a fixture message whose content starts with `SYSTEM: CONTEXT LIMIT EXCEEDED` (the marker the current binary no longer emits, `context.rs:287-309`) comes back byte-identical and is not pruned; `System`-variant rebirth checkpoints stay `System` (§4 R-PIN-3).
8. **AC-M8 engine scalars**: `max_context_tokens`, `rebirth_advisory_emitted`, `consecutive_rebirths` match the committed values, and a deliberately different `Config` budget produces a `warnings` entry naming both numbers without changing either (§4 R-PIN-6).

### 9.2 Pins

9. **AC-P1** `restored.messages[0]` is `MessageRecord::System` and equals the committed value byte-for-byte; `messages[1]` is `User` and equals the committed value byte-for-byte — **after the full boot path has run**, i.e. the assertion is made post-`build_manager_context`, which is what proves R-PIN-2 (the setters were skipped).
10. **AC-P2** `pin_witness.system_prompt_len == messages[0].content.len()` and `goal_len == messages[1].content.len()` on the installed vector.
11. **AC-P3** recomputed `pinned_prefix_end == persisted pinned_prefix_end` after normalization (§8 N3).
12. **AC-P4** the composed-at-boot prompt is *different* from the pinned one (the fixture asserts `fresh_prompt != restored.messages[0].content`), so AC-P1 cannot pass by accident.
13. **AC-P5** a bundle with a truncated or hand-edited pin ⇒ `Refused { code: PinViolation, member: "manager.json" }` and the member's bytes are unchanged (compare a byte hash of the file before and after the attempted restore — uses §7.2's fingerprint, not a second hasher).

### 9.3 Workers

14. **AC-W1** the restored `worker_key` set equals the committed set (modulo the completed-ring cap, with `RepairReport.completed_dropped_by_ring_cap` equal to the difference) — §5.8 I-W1.
15. **AC-W2** per worker, `history` is element-by-element equal (the same comparison as AC-M1..AC-M5, run per key) **and** `history_source == LiveMirror`; a `SeedOnly` fixture must NOT satisfy AC-W2 and must instead produce the reported partial (§5.5 rule 5).
16. **AC-W3** per worker: `role`, `task_id`, `status`, `status_label`, `context_tokens`, `last_tokens`, `counters.*` (`implementation_turns`, `validation_rounds`, `latest_validator_feedback`), `brief`, `snippets`, `image_urls`, `audio_urls`, `recursion_granted` all equal — and an unrecognized `status_label` keeps the label verbatim with `status == Unknown` (§5.3). *(Amendment A-2: that worker is named by a `warnings` sentence carrying the key and the label; it is not listed in the interrupted-projection set — §5.8 I-W7.)*
17. **AC-W4** the live `WORKERS` map contains an entry under each restored key with `completed_seq` equal to the stored value; `is_active()` ⇔ `completed_seq == None` (§5.8 I-W5).

### 9.4 Identity continuity

18. **AC-I1** after restore, `next_worker_id()`-equivalent draws are `> identity_floors.worker_id_seq_floor`; every restored `worker_id_seq_value < floor` (§5.8 I-W2), and no restored key is re-disambiguated with a `#N` suffix (compare the key set literally). *(Amendment A-3: the floor is the first **unissued** id, so the first post-restore draw **equals** the floor and only the second and later draws are `>` it — read this assertion as `>=` for the first draw and `>` for the rest; the other two clauses stand as written.)*
19. **AC-I2** a completion in the new process draws `completed_seq > max(all restored completed_seq)`, and restored completed entries keep their relative order (§5.8 I-W3).
20. **AC-I3** a notice issued after restore has `NEXT_NOTICE_ID` strictly above `identity_floors.notice_id_floor`; no id matches a pre-crash `notice-<n>` (`src/orchestrator/notice.rs:34-39`).
21. **AC-I4** `Manifest.session_id` is unchanged by a restore, and the next commit writes the same `session_id` (no re-minting).

### 9.5 UI panes

22. **AC-U1** `renderer.messages == committed_ui.chat_messages` element-by-element, asserted **before the first draw** (D4, §6.6 I-U1/I-U2). Includes entries containing embedded `\n`, empty strings, and duplicates.
23. **AC-U2** `renderer.status_line == committed_ui.status_line` — the bar is not `"Ready"` (`src/ui/tui/mod.rs:188`, `:897`).
24. **AC-U3** no pane line is produced from the journal: a fixture where a journal `User { text }` and the pane's `"User: {text}"` disagree restores both unchanged, and `extract_steering_history()` still returns the committed pairs (`src/ui/transcript.rs:113`).
25. **AC-U4** a `"[CLI] …"` line produced only by the TUI (`/help`, `/thought`) is present after restore — the journal has no record of it, which is the whole point of D4.

### 9.6 Composer and view state

26. **AC-C1** `renderer.input_text == committed.composer.input_text`, multi-line included, and `renderer.cursor == committed.composer.cursor` (the caret lands on the same char boundary; §6.5 U-chk).
27. **AC-C2** `history`, `history_index`, `input_draft` match.
28. **AC-C3** `chat_scroll`, `plan_scroll`, `subagent_scroll`, `subagent_list_scroll`, `selected_subagent_idx`, `focused_panel`, `show_thought`, `show_subagent_panel` and the three auto-scroll flags match — **or** differ only by the live clamp at `src/ui/tui/events.rs:466-480`, in which case a `warnings` entry naming the offset and both values exists (§6.6 I-U6).
29. **AC-C4** a fixture with an out-of-range `cursor` (past `input_text.len()` or off a char boundary), an out-of-range `selected_subagent_idx`, or an unknown `focused_panel` ⇒ `Refused { code: HeaderManifestDisagreement, member: "ui.json" }` and the file bytes unchanged.

### 9.7 Subagent panel

30. **AC-S1** the restored panel vector equals the committed one in length and order, and per entry `name`, `task_id`, `prompt`, `logs` (element-by-element), `thinking`, `content`, `is_active`, `context_tokens` are equal.
31. **AC-S2** `worked_duration_secs` equals the committed number, and the value is **not** recomputed from `started_at`/`last_activity_at` (fixture: two `TimePoint`s that would give a different delta if subtracted; the assertion proves D5 T3).
32. **AC-S3** `started_at` / `last_activity_at` re-anchor within the documented error term (§5.6 T2) and their `wall` values are byte-exact.
33. **AC-S4** a panel with `is_active: true` restores the byte **and** emits `NonRestorableNotice { kind: JoinHandle, subject: name }` (§6.3).

### 9.8 Time and monotonicity

34. **AC-T1** every restored `TimePoint.elapsed_secs` is measured against the same anchor as `Manifest.anchor_wall`, and ordering of `saved_elapsed_secs` between members matches the committed ordering (§5.6 T4).
35. **AC-T2** a `TimePoint` with `elapsed_secs` NaN/inf/negative ⇒ `Refused`, no defaulting to "now" (§5.6).

### 9.9 Named scenario (a) — abort while a subagent was in flight

36. **AC-A1** Setup: one active worker (`status_label: "In Progress"`, `completed_seq: None`, mirror `history_source: LiveMirror`, ≥1 assistant turn with `tool_calls` all paired), one `SubagentPanel` with `is_active: true` and non-empty `logs`/`thinking`/`content`, a composer draft, a non-zero `chat_scroll`. Abort is requested (the live abort path breaks the tool round at `src/ui/session.rs:1127-1135`) and the session commits `ending: UserAbort`.
    Assertions: AC-M1..AC-M8 and AC-W1..AC-W4 all hold; the worker comes back with `completed_seq == None` so `has_active_workers()` sees it; the subagent panel comes back byte-exact (AC-S1); the composer and scroll come back (AC-C1, AC-C3).
37. **AC-A2** the notices are present and specific: `CancellationToken`, `JoinHandle`, `DropGuard` for each in-flight worker, plus `PendingNotice` for a notice queued in `PENDING_NOTICES`/`WORKER_INBOXES` at abort time (`src/orchestrator/notice.rs:76`, `:81`). The restore is `Restored`, never silently "complete": `report.notices` is non-empty and its `subject` values are the worker keys.
38. **AC-A3** the cancel token of the restored worker is not the old token (it cannot be), and the worker is reported as non-cancellable-until-rearmed (§3.11 `CancellationToken`); nothing in the restore path re-launches the worker (t-006 owns re-launch).

### 9.10 Named scenario (b) — crash leaving a dangling `tool_calls`

39. **AC-D1** Setup: the manager transcript ends with `Assistant { content: None, tool_calls: [c1, c2] }` and exactly one `Tool { tool_call_id: c1 }` — the exact shape written between `src/ui/session.rs:689-690` and the never-reached boundary repair at `:1788-1795`. `ending: InProgress`, `dangling: Dangling { calls: [c1 dispatched: true, c2 dispatched: false] }`.
    Assertions: outcome is `Restored` with `ending == HardKillInferred` (§3.4); installed transcript has `c1`'s real result directly under the assistant and a synthesized `Tool { tool_call_id: c2, content: "(aborted)" }`; `report.aborted_results_synthesized == 1`; `report.normalization_passes == 2`; `report.normalization_stable == true`; the synthesized id appears in `warnings`.
40. **AC-D2** the assistant turn itself is **not** dropped, and `content: None` on it is still `None` (§2.1's trap, and `context.rs:376-383`'s option-(a) decision).
41. **AC-D3** `messages[0]`/`messages[1]` keep their indices and bytes despite the synthesis (§4, §8 N3); `pinned_prefix_end` unchanged.
42. **AC-D4** a `dispatched: true` call raises `NonRestorableNotice { kind: PtyProcess }` naming the worker/manager subject, and no tool is re-executed by the restore path.
43. **AC-D5** an orphan-result variant (a `Tool` whose parent assistant is gone) yields `orphan_tool_results_pruned == 1`, the id in `warnings`, and the rest of the transcript element-by-element equal.
44. **AC-D6** the bundle is byte-identical after the restore: `fingerprint(file)` before and after are equal for every member and for `manifest.json` (D6 read-only rule, §7.3 L9). Normalized bytes appear only on the **next** commit, at the next commit trigger.
45. **AC-D7** two corrupt worker files (flip one byte each, so `fingerprint` mismatches) ⇒ `Refused` and every member byte unchanged; exactly one corrupt worker file ⇒ `PartiallyRestored` with `unrecoverable_workers == [<that key>]`, every other AC in 9.3 holding for the surviving workers (§5.8 I-W6, D6).

---

## 10. Non-restorable inventory — what does not survive, and how that is said out loud

D5 (§1.1) forbids two failure modes at once: silently *losing* a capability, and silently *faking* it back (re-minting a `CancellationToken`, re-attaching a dead PTY, fabricating a `JoinHandle`). D5's answer is that every loss becomes a `NonRestorableNotice` (§3.11) — a first-class field of `WorkerSection::non_restorable` (§3.9) and of `RepairReport::notices` (§3.10), surfaced through the same three loud destinations as every other loud event (§8.4: `tracing::warn!` + status line + journaled `UiRecord::Status`). This section is the **closed inventory**: the complete list of things the bundle cannot reproduce byte-exactly, and the exact representation that stands in for each.

Two rules govern the whole section:

- **N-R1 — a notice never substitutes for bytes.** The persisted bytes are restored as they are (pane strings, `is_active`, `status_label`, `history`) and are *not* edited to reflect what the new process can actually do. The notice only states what is gone. Silently flipping `is_active: true` to `false` to "match reality" is prohibited (§6.3, AC-S4) — that would change the pane, which is the byte authority (D4).
- **N-R2 — the inventory is closed.** If a restore discovers a loss that is not listed below, it SHALL emit `NonRestorableKind::Other` with a `detail` naming the discovered loss, **plus** a `RepairReport::warnings` entry saying the inventory in §10 is incomplete. Discovering an unlisted loss silently is a defect of the same severity as a silent re-mint.

### 10.1 The live process tree: PTY children and process groups — `PtyProcess`

Verified ground truth:

| `path:line` | Fact | Consequence for restore |
|---|---|---|
| `src/harness/pty.rs:1446` | `pub static GLOBAL_PTY_MANAGER: LazyLock<PtyManager>` — one process-global manager; sessions are map entries holding a `PtySession` (`:434`). | The registry is process-memory. Nothing in it survives, and there is no durable record of which children existed. |
| `src/harness/pty.rs:13-16`, `:262`, `:1566` | Teardown is `libc::kill(-pid, libc::SIGKILL)` to the **whole process group** plus a child kill, issued on completion, on the strict 300 s timeout, or at teardown — `kill_process_group` is the single unix site. | That teardown **never runs** on Ctrl-C/`kill`/crash of the parent. Children that outlive us keep mutating the workspace while we believe they are gone. |
| `grep -rn "Pdeathsig\|prctl" src/` | **empty** at this revision: no `Pdeathsig`, no `PR_SET_PDEATHSIG`, anywhere in `src/`. | The OS will *not* kill our children when we die. This is a real lifecycle hole (`docs/research_restore_lifecycle.md` §5-10), and it is data-model-visible precisely because §5.4 records `dispatched: true`. |
| `src/harness/pty.rs:73`, `:982`, `:1021-1042`, `:1079` | `PTY_OUTPUT_BUFFER_CAP = 256 * 1024`; the unread tail is ring-trimmed with a bracketed "earlier output discarded" marker line. | Even the output that *existed* was already lossy and lived in memory. There is no faithful byte to restore, so no pretense of one. |

**Representation.** For each live PTY the restore path knows about — which is derivable only from a `WorkerSection`/`ManagerSection` whose `dangling` is `Dangling { calls }` with `dispatched: true` (§5.4), or from `ProcessEnding` ∈ {`Interrupted`, `Unknown`} together with a worker whose `status_label`/`status` says it was running — emit one `NonRestorableNotice { kind: PtyProcess, subject: <worker key or "manager">, detail: <sentence> }`. The loader SHALL NOT re-run the call, SHALL NOT synthesize success, and SHALL NOT claim the side effect did or did not happen (§8.3 H1). `detail` (exact, user-visible): *"A tool subprocess was running when the process ended; its process group was not killed, its output was not saved, and it will not be re-attached or re-run — files it was writing may be partial."*

**Non-normative note for t-006/t-010:** adding `Pdeathsig(SIGKILL)` at spawn (`unsafe` `CommandExt` on unix) would shrink this class from "may still be mutating the workspace" to "was killed by the OS". That is a **lifecycle** change outside this document's scope; it is recorded here so the inventory can legitimately shrink later, and so nobody mistakes the current notice for an inherent limit.

### 10.2 In-flight provider requests and half-streamed turns — `Other`

| `path:line` | Fact | Consequence |
|---|---|---|
| `src/llm/client.rs:634` | `let mut stream = resp.bytes_stream().eventsource();` — a live SSE stream over a `reqwest` response body. | A half-consumed HTTP response body is a socket, not data. No provider protocol resumes a partially streamed completion. |
| `src/ui/tui/mod.rs:45`, `:47` | `current_thought` / `current_content` hold the partially streamed thinking/content of the turn in flight. | Deliberately **not** persisted (§6.2, §6.5): they are mid-flight render state, and persisting a half-turn would fabricate a message the model never finished. The restored pane therefore ends at the last *completed* line. |
| `src/ui/session.rs:689-690` | the assistant turn (with `tool_calls`) is appended and saved **before** dispatch. | The *request* half survives byte-exactly; only the *response* half is gone. This asymmetry is why §8 must synthesize `ABORTED_TOOL_RESULT` (§8.3 H1) rather than pretend the round completed. |

**Representation.** One `NonRestorableNotice { kind: Other, subject: <worker key or "manager"> }` per turn that was mid-request at capture instant (derivable from `Dangling { calls }` with `dispatched: true`, i.e. the request went out). `detail`: *"An in-flight model request (or network tool call) was interrupted; the partial answer was not saved and will not be resumed."* The restore path SHALL NOT re-issue it automatically — it cannot prove the provider never saw the request, a replay costs money, and for a network tool it may double-apply an external side effect. A tool whose effect is **external** — any registered tool whose implementation performs a network request rather than a local file or process operation (MCP-registered tools are the common case; §3.5 already re-prices their schemas from live MCP state) — belongs to the same class as the PTY case: the request may have been issued, its result was never persisted, and the loader can make no claim either way.

### 10.3 Task handles, cancellation tokens, RAII guards, monotonic clocks — `JoinHandle` / `CancellationToken` / `DropGuard` / `Other`

| `path:line` | Fact | Consequence |
|---|---|---|
| `src/ui/session.rs:852`, `:876`, `:905`, `:923`, `:1494` | five `tokio::task::spawn_blocking` sites driving manager/worker streams. (The task brief cited `860/884/913/934`; the actual sites drift ≈8 lines earlier — §11.5.) | `JoinHandle`s are neither `Serialize` nor cross-process. A restored `inProgress` worker has **no** task; nothing can be joined, aborted, or awaited. |
| `src/orchestrator/workers.rs:39` | `ActiveWorkerInfo::cancel_token: Option<CancellationToken>` (also threaded through `:233/:250/:270/:286/:314`). | A `CancellationToken` from the dead process cannot cancel anything. A fresh token belongs to a *new* execution, not to the restored one (D5). |
| `src/orchestrator/workers.rs:152`, `:154-211` | `ActiveWorkerGuard(pub String)` whose `Drop` flips the entry to completed in place, is deliberately idempotent, and calls `super::notice::reclaim_worker_inbox(&self.0)` (`:211`). | The guard never existed in the new process, so **its bookkeeping will never run**: the worker's notice inbox is not reclaimed by a drop, and the completed-transition side effects are absent. A restore that "constructs a guard" would fire those effects a second time — prohibited. |
| `src/manager/phase.rs:177-182`, `:223` | `PLAN_STARTED_AT: LazyLock<RwLock<Option<(std::time::Instant, DateTime<Local)>>>>`; `PLAN_COMPLETED_AT` is `Instant`-only; durations are computed from `Instant`. | `Instant` is monotonic per process. An elapsed-time read after a restart is meaningless against pre-restart instants. Only the wall-clock half is durable, and it is carried by `TimePoint` (§3.11) and the already-durable `worked_duration_secs` precedent (`src/ui/mod.rs:26`, §6.3) — never re-derived from `Instant`. |

**Representation, per `inProgress` worker** (exactly the list §5.7 step 5 requires): `JoinHandle`, `CancellationToken`, `DropGuard`, plus `PtyProcess` when `dangling.dispatched` is `true`. A fresh `CancellationToken` SHALL be minted **only** if and when t-006 decides to re-launch that worker, and the notice SHALL still be emitted, because the restored worker is not cancellable through anything the old session had. `detail` sentences (exact):

- `JoinHandle` — *"The task driving this subagent ended with the previous process; it cannot be joined or aborted, and its completion bookkeeping will not run."*
- `CancellationToken` — *"The previous cancel control is gone; this subagent cannot be aborted with its old control until it is re-launched."*
- `DropGuard` — *"The completion/eviction bookkeeping for this subagent was tied to a guard that died with the process; those side effects will not run."*

A restored `SubagentPanel { is_active: true }` SHALL emit `JoinHandle` with `subject: <panel name>` even though the pane byte is kept (§6.3, AC-S4).

### 10.4 The volatile notice inboxes — `PendingNotice`

| `path:line` | Fact | Consequence |
|---|---|---|
| `src/orchestrator/notice.rs:76` | `static PENDING_NOTICES: LazyLock<DashMap<String, SteerNotice>>`. | Notices posted but not yet delivered are in memory only. They are gone. |
| `src/orchestrator/notice.rs:81`, `:89-90` | `static WORKER_INBOXES: LazyLock<DashMap<String, Inbox>>` where `Inbox { notices: Vec<SteerNotice>, … }`. | The per-worker queue a running specialist drains between turns is gone; a restored worker will not see a notice it had not yet consumed. |
| `src/orchestrator/notice.rs:84`, `:96-97`, `:148` | `static REPLIED_NOTICES: LazyLock<DashMap<String, StoredReply>>`, capped at `REPLIED_NOTICES_CAP = 64`. | Replies a user typed but that had not been collected into the worker's next turn are gone. |
| `src/orchestrator/notice.rs:34-39` | `NEXT_NOTICE_ID` is a process-global `AtomicU64` starting at 1; ids are `notice-{n}`. | **Identity is restorable, content is not**: §5.7 step 1 raises the floor with `fetch_max(notice_id_floor)`, so ids never alias or repeat — while the notices themselves are not. That split is exactly what D5 asks for. |
| `src/ui/transcript.rs:105-112` | pendingness is delegated to `orchestrator::steer::steering_entry_is_pending`, which consults the volatile queues above. | A restored journal entry may report `pending = false` although it was pending at crash instant. §6.4 already refuses to persist a pending flag for this reason; the deviation is announced, not hidden. |

**Representation.** One `NonRestorableNotice { kind: PendingNotice, subject: <worker key or "manager"> }` per worker with a known-nonempty queue at capture instant (the count itself is not restorable; §5.7 emits the notice unconditionally for `inProgress` workers). `detail`: *"Steering notices queued for this subagent lived in memory and were not saved; the notice id counter was preserved so ids will not be reused."*

> **Module-path correction:** the task brief (and one earlier draft line) writes `src/agents/notice.rs`. The file is **`src/orchestrator/notice.rs`**; §2.4 and §5.7 already cite `:34` and `:76-84` correctly. Recorded as §11.5.

### 10.5 Terminal geometry, OS scrollback, and derived layout — `Other`

| `path:line` | Fact | Consequence |
|---|---|---|
| `src/ui/tui/mod.rs:407`, `:410`, `:951`, `:954`, `:1060`, `:1064` | `enable_raw_mode()` + `EnterAlternateScreen` at start; `LeaveAlternateScreen` + `disable_raw_mode()` on both exit paths. | Everything the user scrolled above our panes lives in the **terminal emulator's** scrollback, not in our process. It is outside the bundle by construction, and no design choice can change that. |
| `src/ui/tui/events.rs:301` | `Ok(TermEvent::Resize(_, _)) => …` recomputes geometry. | The new terminal's size is authoritative. Persisting a geometry would restore a screen the user no longer has (§6.2 U5). |
| `src/ui/tui/mod.rs:100-122`, `:133`, `:66` | per-frame layout caches (`chat_width/height`, `subagent_width/height`, `plan_width/height`, `cached_message_lines`, `cached_rendered_messages`), `plan_max_scroll`, `frame_counter`. | All **derived**. §6.2 U5 refuses to persist them. Consequence worth stating: the pane *string* is restored byte-exactly, but its **wrapped** line count can differ on a different terminal size — which is why the `u16` scroll offsets in `ViewState` (§6.5) are expressed in the renderer's own unit (estimated wrapped display lines, `estimated_chat_lines` `src/ui/tui/render.rs:201` / `estimated_subagent_lines` `:221`) and are re-clamped by the **existing** clamp (`src/ui/tui/events.rs:466-480`) rather than validated by new code. The string stays exact; the geometry is re-derived, and a clamp that moved an offset is recorded in `warnings` (§6.6 I-U6, AC-C3). |
| `src/ui/raw.rs:44-51` | `RawRenderer` holds only a `buffer: Vec<u8>` plus `input_state`. | In raw/headless mode there is nothing to restore: stdout bytes were never in our panes. `chat_messages` remains the byte authority regardless of renderer. |
| `src/manager/context.rs:554-571`, `:419` (this doc) | `stats: Option<Arc<HarnessStats>>` (never attached to the manager engine — `set_stats` has no live caller) and `tool_schema_tokens` are deliberately excluded from `ManagerSection`. | Intervention counters and the tool-schema token charge are re-derived from live state. Listed here to keep the inventory closed; §3.5 already decides it, and the notices are `HarnessStats` and `ToolSchemaCharge` (§3.11). |

`detail` (exact): *"Terminal size, and any scrollback outside the marmel panes, belong to the terminal rather than to the session: the pane text is restored byte-exactly and re-wrapped for the current window."*

### 10.6 The inventory in one table

| Lost thing | Why it cannot survive | `NonRestorableKind` | `subject` convention | `detail` sentence |
|---|---|---|---|---|
| PTY child + process group, its output ring | process-memory manager (`pty.rs:1446`); teardown (`:13-16`, `:1566`) never runs on kill; **no `Pdeathsig` anywhere in `src/`** | `PtyProcess` | worker key, or `manager` | §10.1 sentence |
| In-flight provider request / half-streamed turn, `current_thought`/`current_content` | live SSE body (`client.rs:634`); render state deliberately unpersisted (`tui/mod.rs:45`, `:47`) | `Other` | worker key, or `manager` | §10.2 sentence |
| `JoinHandle` of the driving task | not serializable, process-scoped (`session.rs:852/876/905/923/1494`) | `JoinHandle` | worker key; subagent panel name | §10.3 sentence |
| `CancellationToken` | process-scoped (`workers.rs:39`) | `CancellationToken` | worker key | §10.3 sentence |
| `ActiveWorkerGuard` and its completion/inbox-reclaim effects | `Drop` (`workers.rs:154-211`, `:211`) belongs to a dead frame | `DropGuard` | worker key | §10.3 sentence |
| Queued/replied notices | `PENDING_NOTICES`/`WORKER_INBOXES`/`REPLIED_NOTICES` (`notice.rs:76/81/84`) | `PendingNotice` | worker key | §10.4 sentence |
| `HarnessStats` counters; `tool_schema_tokens` charge | `Arc`, process-shared; re-priced from live MCP set | `HarnessStats`; `ToolSchemaCharge` | `manager` | *"Session counters and the tool-schema token charge are re-derived from live state, not restored."* |
| Worker history that was never mirrored (`SeedOnly`/`NotCaptured`) or an unrecoverable worker file | history lives in a stack local (`execution.rs:175-180`, §2.5) | `WorkerHistory` | worker key | *"This subagent's full message history was not captured; only its seed is restorable."* |
| `Instant`-based durations (plan start/elapsed) | monotonic per process (`phase.rs:177-182`, `:223`) | `Other` | `manager` | *"Elapsed-time figures are re-derived from wall-clock anchors, not from the previous process's clock."* |
| Terminal geometry, OS scrollback outside our panes, layout caches, `plan_max_scroll`, `frame_counter` | owned by the terminal / recomputed per frame (`tui/mod.rs:407/410/100-122/133`, `events.rs:301`) | `Other` | `session` | §10.5 sentence |
| `TextArea` undo/redo stack and in-widget selection anchors | ratatui-textarea internals, not exposed as data (§6.5) | `Other` | `session` | *"The caret and text are restored exactly; the composer's undo history is gone."* |

### 10.7 Emission rules and what §9 will assert

1. **Union, deduplicated by `(kind, subject)`.** Capture-time losses may already be recorded in `WorkerSection::non_restorable` (§3.9 — e.g. `WorkerHistory` for a `SeedOnly` history). The loader SHALL additionally emit load-time notices (§10.1-§10.5) and SHALL write the union into `RepairReport::notices`, deduplicated on the pair so a worker that lost four capabilities gets exactly four notices — never four copies of one.
2. **Zero notices for an `InProgress` session is a defect.** Any bundle whose manifest reports `processEnding != CleanExit` SHALL produce at least one notice; a restore emitting none means the projection lied. (§9 AC-A3, AC-D4 assert the concrete cases.)
3. **Notices are load-time facts, not stored promises.** Nothing in the bundle asserts what a *future* process will be able to do; notices are computed by the loader against the current process. That is what keeps §7 fail-closed semantics independent of §10.
4. **Every notice is loud.** All three destinations of §8.4 apply to notices; a `let _ =` on any of them violates D6.

---

## 11. Audit verdict on §1–§5 (this section substitutes for the missing validation verdict on t-018)

§1–§5 were delivered **without** a validation pass, so this section performs one. Method: the document was read in bounded chunks; **more than 40 anchors** — every §2 table plus every construct §6–§10 depend on — were re-checked against the source at the current revision (branch `1.0.0-refactor`, HEAD `bbfb893`) with `grep`/`sed` at the cited line ±2. Nothing below is inferred from the document alone; each row names what the source actually contains.

**Verdict up front:** §1–§5 **is implementable as written**, with **one real blocker** (11.3-1), **one shape gap** that must be closed before the §9 acceptance list can be machine-checked (11.3-2), **one guarantee that is stated more strongly than the primitive provides** (11.3-3), and **13 citation drifts** of which **2 are material** (they point at the wrong construct). No naming conflict, no semantic contradiction, and no rule that cannot be built was found. Details, then the action list.

### 11.1 Anchors confirmed exactly as cited

| §1–§5 claim | Verified at `bbfb893` |
|---|---|
| `src/manager/context.rs:327` `ABORTED_TOOL_RESULT = "(aborted)"` | exact |
| `:592` `set_system_prompt`, `:604` `set_goal`, `:633` `messages()`, `:751` `compact()`, `:935` `perform_rebirth`, `:270` `pinned_prefix_end`, `:305` `INJECTED_ADVISORY_PREFIXES`, `:314` `is_injected_advisory`, `:92` `REBIRTH_CHECKPOINT_PREFIX` | all exact |
| `src/manager/phase.rs:117`/`:119`/`:298`/`:302`, `:223` `get_plan_start_time` (the `Instant`-re-anchoring precedent) | exact (`pub fn get_plan_start_time` at `:223`; the mtime fallback it describes follows inside `:224-235`) |
| `src/ui/transcript.rs:113` `extract_steering_history`, `:76`, `:89`, `UiRecord` enum at `:14` with the 8-variant set | exact |
| `src/ui/mod.rs:19-37` `SubagentDetail` — field order and types exactly as §2.3 lists them (`started_at: Option<Instant>` `:24`, `worked_duration: Duration` `:26`, `last_activity_at` `:28`, `logs: Vec<String>` `:30`, `thinking` `:31`, `content` `:32`, `is_active` `:34`, `context_tokens` `:36`) | exact |
| `src/ui/tui/mod.rs:43` `messages: Vec<String>`, `:68` `status_line`, `:100` `chat_scroll: u16`, `:102-122` layout caches, `:133` `plan_max_scroll`, `:135` `active_plan_task` | exact |
| `src/orchestrator/workers.rs:103` `MAX_RECENT_COMPLETED = 10`, `:62` `WorkerState` | exact |
| `src/orchestrator/notice.rs:34-39` `NEXT_NOTICE_ID = 1` / `notice-{n}`; `:76` / `:81` / `:84` the three `DashMap` statics | exact |
| `src/harness/fs.rs:475-503` `pub fn write_file_atomic` (staging name `.{stem}.tmp.{pid}.{nanos}.{seq:08}` at `:395`, `REPLACE_LOCKS` 64 shards at `:334-335`, FNV-1a path hash constants `0xcbf2_9ce4_8422_2325` / `0x0000_0100_0000_01b3` at `:352-358`) | exact |
| `src/ui/session.rs:182` `build_manager_context` + `:184` `ctx.set_system_prompt`, `:380` `set_goal`, `:267` `set_thinking_budgets`, `:302` `UiTranscript::new()`, `:319` empty subagents, `:689-690` `ctx.append(assistant)` + `let _ = ctx.save_transcript(...)`, `:1788-1795` `ensure_pairing_at_boundary("after tool round", …)` | exact — including the load-bearing D6 claim that the assistant-with-`tool_calls` is persisted **before** dispatch and the boundary repair runs only after a completed round |
| `src/main.rs:277` `std::panic::set_hook` | exact |
| D2's count of 22 `save_transcript` + 22 `ui_transcript.save` call sites | re-counted: **22 and 22** |
| commit `31f4e90` cited in D2 | exists (`git cat-file -t` → `commit`) |

### 11.2 Line-number drift (concentrated in `src/orchestrator/workers.rs`, plus `src/manager/*`, `src/types/wire.rs`, one `src/ui/session.rs` range)

| §1–§5 citation | Actual at `bbfb893` | Severity | Action |
|---|---|---|---|
| `workers.rs:243-252` — key grammar / collision disambiguation (cited twice: §2.4 and §3.9's `worker_key` doc) | the minting site is **`claim_worker_slot` at `:309`**, `{base}#{id}` at `:329`, UUID fallback at `:334` | **material** (≈66 lines off; the cited window is an unrelated comment block) | repoint to `:309-340` |
| `workers.rs:186-207` (D5, `ActiveWorkerGuard`) | `pub struct ActiveWorkerGuard` at **`:152`**, `impl Drop` **`:154-211`** (the `reclaim_worker_inbox` call the design relies on is at **`:211`**, *outside* the cited range) | **material** (the cited window misses both the struct and the inbox-reclaim side effect) | repoint to `:152-211` |
| `workers.rs:112` `COMPLETED_SEQ` | `:109` | cosmetic (−3) | optional |
| `workers.rs:130` + `:132-134` `WORKER_ID_SEQ` / `next_worker_id` | `:121` and `:124-126` | cosmetic (−9) | optional |
| `workers.rs:137-139` `KEY_DISAMBIGUATOR` | `:134` | cosmetic (−3) | optional |
| `workers.rs:26-40` `ActiveWorkerInfo` | struct at `:28` | cosmetic | optional |
| `workers.rs:66` `last_tokens` | `:68` | cosmetic (+2) | optional |
| `context.rs:339-357` `prune_orphan_tool_messages` | `:339-356` | cosmetic (±1) | optional |
| `context.rs:384-433` `repair_tool_call_pairs` | `:384-434` | cosmetic (±1) | optional |
| `context.rs:554-571` `ContextEngine` | `#[derive]` `:554`, `pub struct` `:555` | cosmetic (±1) | optional |
| `phase.rs:407-421` `Plan::clear` | the **fn** starts at `:381`; `:407-421` is *inside* its body and does land exactly on the transcript/UI `remove_file` blocks the row describes | cosmetic (imprecise address, semantically correct) | optional |
| `types/wire.rs:7-28` / `:73-80` / `:154-159` (`Message` / `ToolCall` / `ToolFunction`) | `:9` / `:74` / `:156` | cosmetic (+1/+2, ranges still contain the constructs) | optional |
| `session.rs:274-280` `load_system_prompt_with_plan_and_warning` | call at `:276`, `build_manager_context` at `:280` | cosmetic (inside range) | optional |

Net effect: **no cited anchor misdescribes the code**; two point somewhere else in the file. Every design consequence attached to those rows still holds (verified against the actual sites, e.g. `COMPLETED_SEQ.fetch_add` really is consumed in `ActiveWorkerGuard::drop` at `:166-167`, and `claim_worker_slot` really is where `#N` disambiguation happens).

### 11.3 Substantive findings

1. **Blocker — §2.5 (`:147`) mandates something currently unreachable.** *"A bundle writer SHALL take the per-path lock for `manifest.json` to serialize concurrent committers."* `replace_path_lock` (`src/harness/fs.rs:342`) is **private** — plain `fn`, used only by `fs::replace` (`:306`) — so `session/` cannot honor that MUST without a visibility change. Two ways out, both compatible with D1: (i) `harness::fs` exposes a `pub fn with_path_lock<T>(path: &Path, f: impl FnOnce() -> T) -> T`; or (ii) `harness::fs` stays untouched and `session/` owns its own commit mutex, with manifest-last commit (§7 C5) as the cross-process guard. **§7.1 prescribes (ii)** and flags the §2.5 wording as unimplementable as written. This is the one item that would have failed at compile time.
2. **Shape gap — `RepairReport` (§3.10) has manager-level before/after counts only** (`manager_messages_before/after`, `ui_lines_restored`, `workers_restored`, `workers_unrecoverable`, `completed_dropped_by_ring_cap`, `notices`, `warnings`). §8's loudness rule needs **per-worker** before/after counts, and there is no field for them, so §8.4 parks them as `warnings` sentences (`"worker <key>: history <before> -> <after>"`). Not a contradiction, but it makes AC-D… style per-worker assertions string-matching. If the §9 test list is to be machine-checkable, `RepairReport` needs a `Vec<WorkerRepairCounts { key, before, after, applied_rules }>` — recorded now so t-010 does not rediscover it.
3. **Over-strong durability wording.** `write_file_atomic` (`fs.rs:475-503`) is `write_all` → `flush` → `rename`, with **no `sync_all`/`fsync`** anywhere in the function (verified). So "atomic" means *atomic with respect to process death* — which is exactly the threat model here (Ctrl-C / `kill` / crash, where the page cache survives) — but **not** durable against whole-machine power loss, where a member can land truncated or empty. The design still holds, because §7's L0–L9 verify-before-parse turns exactly that outcome into `Refused` with the bytes untouched; §7.1 now states the guarantee and its limit explicitly. §2.5's row should say "atomic against process death", not imply media durability.
4. **Ambiguity resolved by t-019, not a contradiction:** `WorkerSection::non_restorable` (§3.9) is a *persisted* field, while §10's notices are *load-time* facts; §1–§5 never say which side fills it. §10.7 rule 1 settles it: capture MAY record capture-time losses, the loader always adds load-time losses, and the union — deduplicated on `(kind, subject)` — is what lands in `RepairReport::notices`.
5. **Completion of §3.6 rather than conflict:** §3.6 declares `UiSection { chat_messages, status_line }`; §6 completes the field set (`subagent_panels`, `journal`, `composer`, `view`, `budgets`) exactly as §1's out-of-scope note assigns it. Neither block derives `Eq`, which is **required**: `SubagentPanel::worked_duration_secs: f64` is not `Eq`, and §6 keeps `UiSection`/`SubagentPanel` at `PartialEq` while the f64-free helpers (`UiJournalRecord`, `ComposerState`, `ViewState`, `BudgetWitness`) do derive `Eq`. Consistent and compile-safe as written.

### 11.4 Internal numbering inconsistency inside §1–§5 (forward references)

Delivered numbering is **§6 = UI section, §7 = write/commit + fingerprint + decision tree, §8 = normalization, §9 = acceptance criteria, §10 = inventory**. Most of §1–§5 already agrees (line 56 "§7" for the per-member fingerprint; line 122/123/126/241/427 use §6 for the UI section and §7 for the commit/fingerprint shape; §3's normalization forward-refs all say §8, which is correct). **Two outliers disagree with the rest of the document:**

- the out-of-scope note (line 14) lists "the write/commit protocol and its ordering guarantees **(§6)**" — that content is in **§7**; the UI field set it does not name is what took §6;
- §5.5 (line 962) says the mirror "is committed by the **bundle protocol (§6, t-019)**" — that is **§7**.

Both should be repointed to §7 in the next editorial pass. (Line 122's provisional name `UiRecordRecord`-style was never fixed by §1–§5; §6 named the type `UiJournalRecord`. No conflict — the original text says "only if t-019's §6 decides".)

### 11.5 Brief-side drift (the document is right; the task brief was stale)

Recorded so nobody "corrects" the doc toward the brief: the brief's `src/agents/notice.rs` is **`src/orchestrator/notice.rs`**; its `spawn_blocking` sites `860/884/913/934` are **`852`/`876`/`905`/`923`** (plus a fifth at `:1494`); its abort break `1135-1138` is **`1127-1135`**. §1–§5 cite none of the brief's wrong numbers, and §6–§10 use the verified ones.

### 11.6 Verdict and action list

- **Implementable as written: yes**, for the data model itself. Every type in §3 compiles in shape (derives are consistent with `f64`/`Instant` exclusions), every MUST names an existing seam, and the six decisions are mutually consistent — no rule in §1–§5 requires something the code cannot be made to do, and no two rules contradict.
- **Must-fix before implementation:** (1) §2.5's per-path-lock MUST — adopt §7.1's session-owned commit lock (or make `replace_path_lock` reachable); (2) decide whether `RepairReport` grows per-worker repair counts (11.3-2) before the §9 list is turned into assertions; (3) downgrade §2.5's atomicity claim to "atomic against process death" (11.3-3).
- **Should-fix (editorial):** the two material line drifts in 11.2 (`workers.rs:243-252` → `:309-340`; `workers.rs:186-207` → `:152-211`) and the two §6/§7 forward-reference outliers in 11.4. The remaining ±1–9 drifts are harmless and listed for completeness.
- **No blocker was found in the vocabulary handoff:** every name §6–§10 uses (`Manifest`, `ManifestMember`, `SectionKind`, `SectionHeader`, `ProcessEnding`, `IdentityFloors`, `ManagerSection`, `PinWitness`, `UiSection`, `MessageRecord`, `ToolCallRecord`, `ToolFunctionRecord`, `WorkerSection`, `WorkerStatus`, `WorkerCounters`, `HistorySource`, `DanglingToolCallState`, `DanglingCall`, `TimePoint`, `RestoreOutcome`, `Refusal`, `RefusalCode`, `RepairReport`, `NonRestorableNotice`, `NonRestorableKind`) is defined in §3 with the meaning used here, and every forward reference §1–§5 made to t-019 now has a delivered section — §6, §7, §8, §9, §10 — so no forward reference in this document dangles.
