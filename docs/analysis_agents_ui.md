# Deep-Dive Analysis: Agent, Validation, and UI Subsystems

**Scope:** `src/agents/*`, `src/ui/*` (incl. `tui/*`), `src/widget.rs`, `src/debug_log.rs`
**Method:** Static reading of all in-scope files plus cross-referencing of the enforcement points they depend on (`src/orchestrator/registry.rs`, `src/orchestrator/mod.rs`, `src/harness/mod.rs`, `src/config.rs`, `src/main.rs`, integration tests in `tests/`).
**Citations:** `file:line` throughout. No source files were modified.

---

## 1. Agent Model — The Five Specialist Roles

### 1.1 Role identity and registry

The role universe is the `Agent` enum (`src/agents/mod.rs:27`): `Coder`, `Researcher`, `Debugger`, `Validator`, `Generalist`, with snake_case serde names and a lenient `from_str` that also accepts the alias `"deepbrain"` for `Generalist` (`src/agents/mod.rs:63-72`).

Role configuration lives in two parallel places that are deliberately kept in lockstep:

1. **The worker structs** — each file declares a unit struct implementing the `Specialist` trait (`src/agents/mod.rs:193`): `name()`, `tool_namespaces()`, a defaulted `run()` (`mod.rs:197-221`), and `may_recurse()` (`mod.rs:222`).
2. **The registry** — `SpecialistRegistry::canonical()` (`src/orchestrator/registry.rs:37-140`) re-declares the identical tool lists per role. A dedicated test asserts the registry allowlist **must equal** the worker's `tool_namespaces()` (`registry.rs:212-227`), making the registry the single routing authority ("the registry is the authority", `registry.rs:3-4`).

Runtime gating is enforced at dispatch, not at declaration: `harness::dispatch_specialist` normalizes the tool name and rejects calls with `ToolError::Forbidden` unless `caller_allows_tool(agent, tool, registry)` passes (`src/harness/mod.rs:579-585`; implementation `src/orchestrator/mod.rs:702-709`). `create_plan` is Manager-only — it is unconditionally forbidden to specialists (`src/harness/mod.rs:559-564`).

### 1.2 Per-role configuration

| Role | Struct / file | Declared tool set | Recursion | Notes |
|---|---|---|---|---|
| **Coder** — "Lead Software Engineer" | `Coder` (`src/agents/coder.rs:11`) | `delegate_task`, `write_file`, `replace`, `read_file`, `run_command`, `grep_search`, `glob`, `rebirth`, `sleep`, `terminal__sleep` (`coder.rs:22-34`) | `true` (`coder.rs:43`) | Doc header claims namespaces `cli_*`, `terminal__*`, `kiwix__*`, `pdf__*`, `memory__*`, `puppeteer__*` (`coder.rs:4-6`) — **not** what the code grants (see §6.1). |
| **Researcher** — "Deep Knowledge & Archival" | `Researcher` (`src/agents/researcher.rs:12`) | Identical generic set to Coder (`researcher.rs:21-32`) | `false` | Doc header promises `kiwix__*`, `pdf__*`, `riksarkivet__*`, `scb__*`, `brave_search__*` (`researcher.rs:4-6`) — not present in code. |
| **Debugger** — "Low-Level Systems Debugger" | `Debugger` (`src/agents/debugger.rs:13`) | Generic set **plus** PTY tools: `pty_spawn/write/read/close/list`, `pty__*`, `pty_*` (`debugger.rs:21-38`) | `false` | Doc header promises `kiwix__*`, `memory__*` (`debugger.rs:4-5`) — not present. |
| **Validator** — "Independent Quality Auditor" | `Validator` (`src/agents/validator.rs:11`) | Read-only inspection set: `read_file`, `grep_search`, `glob`, PTY tools, `leave_verdict`, `rebirth`, `sleep` — **no** `write_file`, `replace`, `run_command` (`validator.rs:29-45`) | `false` | The only role with a dedicated verdict tool. Its tests assert the absence of mutation tools (`validator.rs:66-67, 72-73`). |
| **Generalist** — "Supreme Polymath" | `Generalist` (`src/agents/generalist.rs:11`) | `["*"]` — universal grant (`generalist.rs:22-24`) | `true` (`generalist.rs:26`) | `SpecialistEntry::allows` treats `"*"` as grant-everything (`registry.rs:177-180`). |

Role *system prompts* are statically embedded at compile time via `include_str!` from `prompts/<role>.md` (e.g. `coder.rs:14`, `debugger.rs:16`, `researcher.rs:15`, `generalist.rs:14`). The Validator uniquely ships **five** prompts: a generic one plus per-target audit prompts for coder/debugger/researcher/generalist deliverables (`validator.rs:14-23`).

Per-specialist runtime overrides come from `[orchestration.specialists.<role>]` in config (`src/config.rs:59-88`): `model`, `backend_url`, `auth_token`, `validator_model`, `validator_backend_url`, `validator_auth_token`, `max_validator_iterations`, `enable_validator` (aliases `auto_validate`/`enable_validation`), and `mcp_servers`. MCP tools are appended to a specialist's schema at run time from its `mcp_servers` list (`src/agents/runner.rs:323-330`), which is how the doc-header namespaces (kiwix/pdf/…) can actually materialize — but only via config, never via the hardcoded allowlist.

### 1.3 Isolation contract

A delegation is described by `DelegationRequest` (`src/agents/mod.rs:132-152`): `agent_name`, self-contained English `prompt`, bounded `snippets`, optional `task_id` (enables automatic plan check-off), optional `image_urls`/`audio_urls`, and `recursion_granted`. The specialist receives only an `IsolatedContext` (`mod.rs:156-169`) — role system prompt + brief + snippets + media — never the parent conversation. The context engine is built via `ContextEngineFactory::specialist_context` (`mod.rs:171-175`).

The deliverable is `Deliverable { marker, content, task_id }` (`mod.rs:185-190`), where `marker` is a `MissionMarker` (`mod.rs:80-89`):

- `Complete { task_id }` — extracted from a `(t-xxx)` / `[t-xxx]` token by `find_task_id` (`mod.rs:91-96`);
- `Failed { reason }`;
- `Replan { reason }` — "task could not be completed AND the plan/goal needs revisiting".

Marker parsing (`MissionMarker::parse`, `mod.rs:110-129`) is precedence-ordered: `REPLAN REQUIRED` > `MISSION COMPLETE` > `FAILED`, with a sanitization step so benign test output like `15 passed; 0 failed` is not misread as a failure (`mod.rs:98-108`). This is well covered by tests (`mod.rs:244-295`).

### 1.4 Role gating / recursion — declared vs. enforced

`Specialist::may_recurse()` (`mod.rs:222-224`) is set `true` for Coder and Generalist, `false` for the rest — but a workspace-wide grep shows **no production code ever consults it**; it is exercised only in the embedded unit tests (`coder.rs:58`, `generalist.rs:40`, etc.). Actual recursion control is:

- an **unconditional fractal depth gate** in `OrchestratorManager::delegate` (`src/orchestrator/mod.rs:269-289`), which rejects delegations beyond `max_recursion_depth` regardless of `recursion_granted` (the comment at `mod.rs:273-282` explicitly documents this choice);
- `delegate_task` being a normal allowlisted tool: a specialist may recurse only if its registry entry grants `delegate_task` (all five do, per `registry.rs:41-152`).

Consequently `DelegationRequest::recursion_granted` (`mod.rs:151`) is dead weight: every production call site hardcodes `false` (`src/manager/loop.rs:725`, `src/orchestrator/steer.rs:565`, `src/orchestrator/mod.rs:512`).

---

## 2. Agent Lifecycle — `runner.rs` End-to-End

### 2.1 Entry points and the test/live split

`Specialist::run()` (default impl, `mod.rs:197-221`) first honors the cancellation token (returning a `Failed (aborted)` deliverable), then calls `run_specialist_llm` (`src/agents/runner.rs:10-39`) and parses the returned text into a marker; a missing marker degrades to `Failed { reason: "no terminal marker" }` (`mod.rs:212-215`).

`run_specialist_llm` builds a *canned* deliverable (role, brief, snippet block, `MISSION COMPLETE`, `runner.rs:15-33`) and then attempts `try_run_specialist_live` (`runner.rs:41-77`). The live path is skipped when:

- there is no tokio runtime (`Handle::try_current` fails, `runner.rs:43-45`);
- the process is a cargo-test binary (current exe path contains `/deps/`) and `MARMEL_LIVE_TEST` is unset (`runner.rs:46-54`);
- config cannot be loaded or the resolved backend URL is empty (`runner.rs:55-58`).

Otherwise it resolves per-specialist `backend_url`/`auth_token`/`model` overrides (`runner.rs:57-68`) and enters `run_specialist_live`. Any error there becomes `"...FAILED"` text (`runner.rs:73-75`). **Note the failure mode:** if config silently fails to load in a non-test environment, the canned `MISSION COMPLETE` string is returned as a fake success (§6.8).

### 2.2 Prompt assembly (`run_specialist_live`, `runner.rs:280-845`)

1. **System prompt** = role prompt + environment block + a hard-coded tool/`rebirth` directive (`runner.rs:289-296`).
2. **Context engine** = `ContextEngineFactory::new(cfg.max_context_tokens).specialist_context(...)` (`runner.rs:298-300`); snippets are appended as a user message (`runner.rs:302-306`).
3. **Model** = specialist override or global (`runner.rs:309-315`); the worker tag is `agent` or `agent-taskid` (`runner.rs:317-323`).
4. **Tool schema** = `ToolDef::default_tools()` filtered by `registry.resolve(agent).allows(...)` (`runner.rs:326-331`), plus MCP tools for the specialist's configured servers (`runner.rs:323-330`).
5. **Worker registration** — `register_active_worker(task_id, agent, brief)` produces a guard that feeds the orchestrator's live-worker table (`runner.rs:334-337`), later updated with turn number, validator iteration, critique, context tokens, and status strings.
6. **Monitoring** — a `HarnessMonitor` + `RepetitionDetector` from `cfg.monitoring` (`runner.rs:339-347`).

### 2.3 The turn loop

Each iteration (`runner.rs:364-786`):

1. **Cancellation check** → `FAILED (aborted)` (`runner.rs:366-372`).
2. **LLM call** — `ChatRequest` with streaming enabled, per-config sampling params (`runner.rs:378-395`), through `chat_stream_resumable` with a `PreemptibleStreamSink` (steer preemption) and the repetition detector (`runner.rs:396-406`). Abort/steer-abort during the call → `FAILED (aborted)` (`runner.rs:408-425`).
3. **Output accounting** — `update_revision` appends non-duplicate reply content to `final_content` (`runner.rs:225-233`); XML-rescue can recover tool calls from raw text when enabled (`runner.rs:437-443`).
4. **Assistant message appended** to the engine (`runner.rs:445-457`).
5. **No-tool-call handling** (`runner.rs:468-554`), in priority order:
   - *budget exceeded* → replace last assistant msg with a truncation notice + corrective SYSTEM NOTICE, max 2 nudges (`runner.rs:469-488`);
   - *repetition detected* → reset the detector, inject an anti-loop notice, max 2 nudges, then break (`runner.rs:489-517`);
   - *no terminal marker* → nudge to use tools or conclude, max 2, then break (`runner.rs:519-540`);
   - *terminal marker but zero tool executions* → fail immediately **without validation**: `assemble_final_deliverable(false, Some("Specialist generated conversational text without executing any tools."), ...)` (`runner.rs:546-554`).
6. **Validation gate** (`runner.rs:568-681`) — see §3.
7. **Tool dispatch** (`runner.rs:683-786`): for each tool call, parse args, emit a status line `agent: tool(preview)` (`runner.rs:690-700`), log full args, consult `monitor.observe_tool` — `Block`/`Cut` interventions substitute an error message instead of executing (`runner.rs:711-724`). Real execution goes through `harness::dispatch_for_with_engine` with `ToolCaller::Specialist(agent)`, wrapped in `tokio::task::block_in_place` on multi-thread runtimes (`runner.rs:726-748`). Successful runs increment `tools_executed_count` and reset the nudge counter (`runner.rs:750-757`). A successful `rebirth` result is **not** appended to the engine (it rewrites context instead, `runner.rs:775-778`). After each tool: compaction or rebirth advisory, and a context-token progress update (`runner.rs:780-785`).

### 2.4 Termination and deliverable assembly

After the loop (`runner.rs:792-845`):

- zero tool executions and no terminal marker → hard fail without validation (`runner.rs:798-810`);
- worker status set to `Approved` / `Rejected` / `Completed` (`runner.rs:815-819`);
- `assemble_final_deliverable(validation_passed, critique, final_content, task_id)` (`runner.rs:236-278`) applies the marker semantics:

| Validation | Content state | Result |
|---|---|---|
| passed | contains `REPLAN REQUIRED` | returned verbatim — replan survives approval (`runner.rs:247-249`) |
| passed | empty | `FAILED (incomplete)` (`runner.rs:250-252`) |
| passed | otherwise | `MISSION COMPLETE (t-xxx)` appended if missing (`runner.rs:253-261`) |
| failed | any | `VALIDATOR REJECTION: <critique>` header, every literal `MISSION COMPLETE`/`mission complete` rewritten to `REVOKED` (`runner.rs:270-272`), and `FAILED (Validator rejected deliverable)` appended if no marker remains (`runner.rs:264-277`) |

The empty-`final_content` fallback synthesizes `"Specialist executed N tool operations..."` when tools ran but nothing was said (`runner.rs:828-840`).

**REPLAN semantics** flow back to the plan layer: `manager/phase.rs` independently re-parses deliverables with its own `MissionMarker::parse` (`src/manager/phase.rs:546`), so `Complete` auto-checks the plan task, `Failed` records failure, and `Replan` signals the plan needs revision.

---

## 3. Validation Loop — `validator.rs` + `validation.rs`

### 3.1 Trigger and configuration

Automated validation is enabled per specialist via `enable_validator` (default `true`) and bounded by `max_validator_iterations` (default 5) (`runner.rs:353-361`). The initial `validation_passed` flag is true when validation is disabled, iterations are zero, **or the agent under test is itself the Validator** (`runner.rs:360-361`) — validators do not audit their own work.

The gate fires when the specialist loop reaches a terminal state with a non-empty deliverable and at least one tool execution or an explicit terminal marker, and the content is not a replan (`runner.rs:568-573`).

### 3.2 The audit run (`run_automated_validation`, `src/agents/validation.rs:28-378`)

1. **Prompt selection** — per-target audit prompt: coder/debugger/researcher/generalist get their dedicated validator prompts; anything else gets the generic one (`validation.rs:36-43`).
2. **Dedicated backend** — the validator client is resolved from (in order) the *target specialist's* `validator_backend_url`/`validator_auth_token`/`validator_model` overrides, then the validator role's own overrides, then global config (`validation.rs:45-61`). This lets teams run audits on a different (e.g. stronger) model than the worker.
3. **Brief** — task brief + full deliverable + instructions: inspect with `read_file`/`grep_search`/`glob`/`pty_*`; you are an auditor and *cannot* `run_command` or modify files; you MUST call `leave_verdict` with `verdict` (`APPROVED`|`REJECTED`) and `comments`; use `rebirth` at ≥80% context (`validation.rs:66-74`).
4. **Tool schema** — filtered by the Validator registry entry, plus MCP tools from the validator's or target's `mcp_servers` (`validation.rs:79-95`).
5. **Loop** — same streaming machinery as specialists, but with `temperature: 0.0` for determinism (`validation.rs:168`) and its own active-worker registration (`validator-<agent>`, `validation.rs:97-100`), repetition detection, and XML rescue.

### 3.3 Verdict extraction

- A `leave_verdict` tool call ends validation immediately: `verdict` is compared case-insensitively to `APPROVED` (`validation.rs:230`); comments are accepted from an unusually generous set of fallback keys (`comments`, `comment`, `feedback`, `reason`, `critique`, `details`, `explanation` — `validation.rs:216-227`). Missing comments produce default texts (`validation.rs:232-236`). The verdict is persisted via `debug_log::log_validation_verdict` (`validation.rs:239`).
- If the validator produces **no tool calls**, it is nudged up to 3 times to submit a verdict (`validation.rs:249-260`). After 3 reminders — or if the turn loop simply ends without a verdict — validation is **assumed approved** with an explanatory comment (`validation.rs:262-267`, `validation.rs:366-378`).

### 3.4 Feedback and re-work cycle

Back in `runner.rs`:

- **Approved** → `validation_passed = true`, critique (or "All verification checks passed.") recorded, worker status `Approved`, loop breaks (`runner.rs:586-604`).
- **Rejected** → critique stored, status `Rejected`/`Revising (rejected pass i/n)`, and a feedback message is appended to the *specialist's* engine: "Validation feedback: The validator tested your changes and found issues: … Please address all validator critique points… conclude with 'MISSION COMPLETE'." (`runner.rs:605-655`). The nudge counter is reset so the specialist gets fresh nudges, and the loop continues — this is the re-work cycle.
- **Validator infrastructure error** → `break` out of the validation branch (`runner.rs:656-666`), leaving `validation_passed` at its initial value; for a non-validator specialist that initial value is `false`, so the deliverable is rejected with whatever critique exists.
- **Iteration budget exhausted** → `validation_passed = false`, status `Failed (max validator iterations exceeded)` (`runner.rs:668-676`).

The final rejection path is the `REVOKED` rewrite in `assemble_final_deliverable` (§2.4), which is unit-tested including the implicit-approval and replan-preservation cases (`src/agents/mod.rs:298-336`).

### 3.5 Assessment of the loop

The design is genuinely closed-loop (specialist → audit → targeted feedback → revision → re-audit, bounded by `max_validator_iterations`), and the validator's read-only tool set plus `temperature: 0.0` are sound choices. The dominant weakness is that the loop is **fail-open**: every path where the validator fails to emit a verdict (3 nudges exhausted, loop end, missing `verdict` argument defaulting to `APPROVED` at `validation.rs:213-215`) approves the deliverable. This directly contradicts the Validator's charter — "never inflates pass status" (`validator.rs:5-6`) — because an infrastructural failure inflates pass status silently. A fail-closed default (or at least a distinct `Inconclusive` outcome) would match the charter better. Note also that `ValidationOutcome` (`validation.rs:9-26`) is defined and exported but never used by the actual flow, which returns a raw `(bool, String)` tuple (`validation.rs:28`, `runner.rs:585`).

---

## 4. UI Layering

### 4.1 The contract: `ui/mod.rs`

The UI defines three shared types and one trait:

- `SubagentDetail` (`src/ui/mod.rs:17-39`) — per-specialist live state: name (`agent-taskid`), task id, prompt, timing (`started_at`, `worked_duration`, `last_activity_at`), `logs`, streaming `thinking`/`content`, `is_active`, `context_tokens`.
- `Event` (`mod.rs:41-63`) — the single event vocabulary: `Message`, `SteerResponse`, `Thinking`, `ToolCall`, `ToolResult`, `Status`, `Delegation(DelegationEvent)`, `Done`, `TokensIn`, `TokensOut`.
- `Renderer` (`mod.rs:65-79`) — the abstraction both front-ends implement: `init/on_event/flush/poll_input/read_input/request_abort/aborted/clear_abort/shutdown` plus optional `set_subagents`, `rehydrate_messages`, `rehydrate_subagents`.

`session::run_session` (`src/ui/session.rs:18-925`) is the only driver; `main.rs` selects the front-end (`--raw` flag, `ui_mode` config, or non-TTY stdout → raw; else TUI — `src/main.rs:69-88`).

### 4.2 `raw.rs` — headless renderer

`RawRenderer` (`src/ui/raw.rs:44-206`) buffers labelled lines (`[assistant]`, `[thinking]`, `[tool]`, `[tool-result]`, `[status]`, `[delegation]`, `[steer]`, `[done]`) chunked at 512 bytes via `chunk_utf8` for script-parseability (`raw.rs:58-65`, `raw.rs:76-115`). It never touches terminal state (its `restore()` is a defensive no-op, `raw.rs:25-40`), never polls stdin (`raw.rs:117-128`), and rehydrates a transcript by replaying messages — collapsing `delegate_task` tool results to their first `MISSION COMPLETE` line (`raw.rs:146-205`). Designed for `marmel --raw "goal" | script`.

### 4.3 `bridge.rs` — streaming sink + steer arbitration

The bridge is the meeting point of three worlds: LLM streaming, the renderer, and the steering arbitrator.

- `RendererSink` (`src/ui/bridge.rs:360-421`) implements the LLM layer's `StreamSink`: `emit` maps `StreamEvent::Content/Thinking/Status` to renderer events and flushes per chunk (`bridge.rs:377-389`); `is_aborted` merges renderer abort and steer abort (`bridge.rs:390-392`); `poll_control` drains arbitration events, then classifies user input into `Abort` (slash commands), `Pause { user_input }` (mid-flight steer), or `Continue` (`bridge.rs:391-421`).
- `on_pause` (`bridge.rs:423-588`) is the synchronous steering path: it builds a `SteerContext` (goal, plan content, plan progress, active subtasks, steering history), preempts conflicting streams, runs `arbitrate_steer_context_stream`, and then interprets the decision: `RespondDirectly` → resume; `AbortImmediately`/`RejectPlan` → abort + queue; `ForwardToWorker`/`ApprovePlan`/default → queue + resume; `Sleep` → sleep up to 300 s, cancellable via the global token (`bridge.rs:529-556`); `DelegateTask` → executes subtasks inline via `execute_steer_subtask`, emits delegation events, and queues deliverable summaries for the next orchestrator turn (`bridge.rs:497-528`).
- `spawn_steer_arbitration` (`bridge.rs:35-215`) is the *asynchronous* twin used while tools run in the background: it sends `SteerArbEvent`s (Delta / DelegationStarted / DelegationCompleted / SynthesizedAnswer / Finished) over an unbounded channel, optionally synthesizes a direct answer from completed deliverables, and appends to the shared steering history (`bridge.rs:186-199`).
- `drain_steer_arbitration_events` (`bridge.rs:218-345`) pumps that channel into the renderer and the `steer_queue`, updating subagent lifecycle along the way.

### 4.4 `session.rs` — the conductor

`run_session` wires everything (`src/ui/session.rs:18-925`):

1. **Startup** — installs global status/event senders (`session.rs:35-38`), logs session start (`session.rs:26`), recovers a Deep-Freeze checkpoint if the journal is frozen, streaming recovery progress to the renderer (`session.rs:41-89`).
2. **Rehydration** — loads the transcript, replays it to the renderer (`session.rs:91-102`), folds the recovered deliverable into context as a SYSTEM NOTICE (`session.rs:104-111`), and rebuilds the subagent list from transcript + crash journal + recovered deliverable + plan checkboxes via `rehydrate_subagents` (`src/ui/helpers.rs:270-455`).
3. **Goal acquisition** — initial argument, or interactive loop handling `/abort`-family and `/reset`-family commands and pending-plan resume (`session.rs:136-160`).
4. **Main loop** (`session.rs:236-920`): per turn it drains status/event/steer/delegation channels, polls renderer input (abort/reset/steer), enforces `MAX_TURNS` (`session.rs:275-277`), compacts context or injects rebirth advisories, and runs one LLM turn through `RendererSink`.
5. **Tool execution** — two paths: a **parallel** path when all calls are `delegate_task`/read-only (`all_parallel`, `session.rs:411-660`), spawning each dispatch on `spawn_blocking` and polling with 20 ms timeouts while continuously draining UI channels and accepting new steering; and a **sequential** path otherwise (`session.rs:640-846`) with the same polling structure. Delegation results auto-check-off the plan task (`session.rs:577-580`, `session.rs:817-820`) and update subagent lifecycle.
6. **Completion** — when the plan is complete, a SYSTEM NOTICE orders final synthesis with no further tools (`session.rs:848-854`). Steer-requested aborts *do not* end the session: abort state is cleared and the next turn starts with the queued steering in context (`session.rs:863-893`); a plain renderer abort breaks out (`session.rs:895-897`). The loop ends with `Event::Done` + shutdown (`session.rs:922-925`).

### 4.5 TUI modules — what runs where

- **`tui/mod.rs`** — `TuiRenderer` state (~40 fields: transcript, streaming buffers, plan panel, subagent panel, input/cursor/history, scroll caches, token counters — `src/ui/tui/mod.rs:37-219`) and the `Renderer` impl (`mod.rs:250-856`). `on_event` (`mod.rs:271-616`) is the event router: token accounting via tiktoken, streaming buffers for Manager vs. the active subagent, log routing by string-prefix matching of agent tags (`mod.rs:390-421`), delegation lifecycle handling with `active_agent` switching and plan-panel auto-open (`mod.rs:424-548`). `flush` draws at a ~16 ms cadence (`mod.rs:618-633`); `read_input` blocks on crossterm until a line is submitted (`mod.rs:644-663`). `set_subagents` merges the session loop's authoritative list while preserving locally streamed thinking/content (`mod.rs:687-766`).
- **`tui/events.rs`** — all input: crossterm keyboard/mouse/resize handling (`handle_events`, `events.rs:15-165`), two-step abort arming (Esc/Ctrl+C, `events.rs:22-45, 96-110`), panel focus cycling (Tab), grapheme-accurate cursor editing (`events.rs:168-246`), click-to-cursor (`events.rs:230-246`), scroll state machines with auto-scroll re-arm (`events.rs:258-356`), subagent upsert (`events.rs:459-497`), history navigation (`events.rs:507-539`), `commit_turn_content` (folds streaming buffers into the transcript, `events.rs:541-570`), and `submit` (local `/thought`, `/help`, `/reset` handling; history push; token accounting; forwarding to the session loop via the `tx` channel, `events.rs:572-660`).
- **`tui/formatting.rs`** — pure functions: prefix-based message styling (`message_style`, `formatting.rs:11-46`), stateful `<think>`/`</think>` segment parsing (`formatting.rs:57-101`), LaTeX→Unicode terminal math (`format_terminal_math`/`format_math_expr`, `formatting.rs:110-297`), the grapheme-aware word-wrap line counter `wrapped_lines` (`formatting.rs:340-424`), plan-scroll helpers `visual_line_offset_of_first_pending`/`_of_task` (`formatting.rs:427-500`), and the sentence splitter feeding steer streaming (`extract_complete_sentences`, `formatting.rs:524-585`).
- **`tui/render.rs`** — frame drawing: incremental wrapped-line cache (`ensure_message_cache`, `render.rs:22-58`), the layout tree in `draw` (vertical main/status/input; horizontal 60/40 chat + right panel; right panel split 50/50 plan/subagents — `render.rs:145-307`), panel renderers `render_chat`/`render_plan`/`render_subagents`/`render_status`/`render_input` (`render.rs:309-990`). `draw` refreshes plan content from disk with a 250 ms mtime-based cache and auto-opens the plan panel (`render.rs:151-196`).

### 4.6 Event flow, backend → terminal

```
LLM stream ──StreamSink──▶ RendererSink (bridge.rs:377) ──Event──▶ renderer.on_event
Specialist runner ──emit_status──▶ global status_tx (session.rs:35) ──drain──▶ Event::Status
Orchestrator delegation ──delegation_events queue──▶ drain_delegation_events (helpers.rs:244) ──▶ Event::Delegation
User mid-flight input ──StreamControl::Pause──▶ on_pause arbitration (bridge.rs:423)  ┐
User input while tools run ──spawn_steer_arbitration (bridge.rs:35)──▶ SteerArbEvent ┘─▶ drain (bridge.rs:218) ──▶ renderer + steer_queue
TUI keyboard/mouse ──handle_events (events.rs:15)──▶ submit (events.rs:572)──▶ tx channel──▶ poll_input/read_input (session.rs)
TuiRenderer.flush (mod.rs:618) ──▶ ratatui Terminal ──▶ CrosstermBackend ──▶ stdout
```

`widget.rs` participates in **none** of this — see §6.10.

### 4.7 `widget.rs` role

Despite the name, `src/widget.rs` is not a Ratatui widget. It is a self-contained parser for a small declarative widget-definition language (`widget NAME KIND { prop = value }`, grammar documented at `src/widget.rs:9-45`), producing `Widget { name, kind, props }` values with `WidgetKind` variants Paragraph/Block/Gauge/List/Table/Chart/Sparkline/Canvas (`widget.rs:82-120`) and line-numbered `ParseError`s (`widget.rs:122-149`). It has no I/O and is unit-tested. However, it is **not declared as a module in `lib.rs` or `main.rs` and nothing references it** (greps for `mod widget`, `crate::widget`, `widget::` return zero hits), so it is not even compiled into the crate — an orphaned, dead source file. The actual UI widgets are built directly with Ratatui primitives in `render.rs`.

---

## 5. `debug_log.rs` — What It Logs and Where

**Activation:** only when `--debug` is passed (or `debug = true` in config): `main.rs:55-66` calls `debug_log::init(Some(<workspace root>/debug.log))`. `init` (`src/debug_log.rs:20-46`) resolves the path (default: workspace root + `debug.log`, else CWD `debug.log` — `debug_log.rs:17, 22-25`), creates parent dirs, opens the file in append mode, stores a `Mutex<File>` in a `OnceLock` (`debug_log.rs:11-13`), and writes a `MARMEL DEBUG LOG STARTED` banner with UTC timestamp and PID. Every logging function is a no-op unless `is_enabled()` (`debug_log.rs:49-58`); writes go through `log_raw` (mutex + flush, `debug_log.rs:61-72`).

**What it logs** (all timestamped, delimiter-banner formatted):

| Function | Content | Primary call sites |
|---|---|---|
| `log_llm_request` (`debug_log.rs:75`) | full outgoing HTTP payload (messages/tools/options), pretty JSON | `src/llm/client.rs:215` |
| `log_llm_response` (`debug_log.rs:92`) | status, elapsed ms, reasoning, content, tool calls | `src/llm/client.rs:327, 341, 413` |
| `log_llm_error` (`debug_log.rs:129`) | URL, model, elapsed, error | `src/llm/client.rs:232, 245, 267` |
| `log_tool_invocation` / `log_tool_result` (`debug_log.rs:143, 158`) | caller, tool, args / elapsed, output, OK/ERROR | `src/harness/mod.rs:448, 462, 469` |
| `log_session_start` (`debug_log.rs:179`) | mode, PID, backend, model, context tokens, specialist keys, MCP servers | `src/ui/session.rs:26` |
| `log_user_input` / `log_user_output` (`debug_log.rs:199, 213`) | source-tagged inputs (initial arg, interactive goal, mid-flight steer, commands) and assistant replies | `src/ui/session.rs:166-911`, `src/ui/bridge.rs:402-412`, `session.rs:361` |
| `log_delegation_start` / `log_delegation_finish` (`debug_log.rs:227, 247`) | agent, task id, snippet count, full prompt / marker, elapsed, full deliverable | `src/orchestrator/mod.rs:304, 353` |
| `log_validation_verdict` (`debug_log.rs:268`) | APPROVED/REJECTED + critique per target agent | `src/agents/validation.rs:239` |
| `log_plan_update` (`debug_log.rs:283`) | plan create / check-off / archive | `src/orchestrator/mod.rs:262, 453` |
| `log_mcp_request` / `log_mcp_response` (`debug_log.rs:297, 314`) | JSON-RPC method, params, elapsed, result | `src/mcp/client.rs:123, 150, 173` |
| `log_stream_pause` / `log_stream_resume` / `log_stream_abort` (`debug_log.rs:339, 353, 367`) | steering pause/resume/abort with token/char positions | `src/llm/stream.rs:562, 569, 582` |
| `log_custom` (`debug_log.rs:329`) | arbitrary tagged entries | misc |

In short: with `--debug`, the file captures the complete I/O surface of the system — every LLM request/response/error, every tool call and result, every delegation, every verdict, every plan mutation, every MCP exchange, every steering pause/resume/abort, and every user input/output — to a single append-only `debug.log` in the workspace root.

---

## 6. Observations

### 6.1 Strengths

1. **Real context isolation.** Specialists see only role prompt + brief + snippets (`IsolatedContext`, `mod.rs:156-175`); the parent conversation never leaks. This is the core of the delegation model and it is consistently honored.
2. **Single-source tool gating with a consistency test.** The registry is authoritative (`registry.rs:3-4`), dispatch enforces it (`harness/mod.rs:579-585`), and a test fails the build if worker namespaces ever diverge from the registry (`registry.rs:212-227`). Defense in depth done right.
3. **Defensive marker parsing.** Precedence ordering, `0 failed` sanitization, and task-id extraction are all tested (`mod.rs:244-295`), and the plan layer mirrors the same parser (`manager/phase.rs:546`).
4. **Bounded, targeted validation.** Per-target audit prompts (`validator.rs:14-23`), a dedicated validator backend/model (`validation.rs:45-61`), `temperature: 0.0` (`validation.rs:168`), and a hard iteration cap (`runner.rs:353-361`) make the loop predictable and auditable via `log_validation_verdict`.
5. **Robust run-loop hygiene.** Budget nudges, repetition detection/intervention, XML rescue, no-tool hard-fail, and cancellation checks at every stage (`runner.rs:437-554, 683-786`) make the specialist loop hard to stall.
6. **Clean renderer abstraction.** The `Renderer` trait (`ui/mod.rs:65-79`) lets the same `run_session` drive both a pipe-friendly raw mode and a rich TUI; `RawRenderer` never mutates terminal state (`raw.rs:25-40`).
7. **Performance-aware TUI.** Incremental wrapped-line caching keyed on width/`show_thought` (`render.rs:22-74`), mtime-throttled plan disk reads (`render.rs:151-160`), and ~16 ms render cadence (`tui/mod.rs:601-616`) show real care for streaming workloads.
8. **Excellent debug observability.** `debug_log.rs` covers the entire pipeline in one opt-in file with consistent formatting and cheap no-op gating.

### 6.2 Weaknesses and code smells

1. **Doc/code drift in role headers.** `coder.rs:4-6` promises `cli_*`, `kiwix__*`, `pdf__*`, `memory__*`, `puppeteer__*`; `debugger.rs:4-5` promises `kiwix__*`, `memory__*`; `researcher.rs:4-6` promises `kiwix__*`, `pdf__*`, `riksarkivet__*`, `scb__*`, `brave_search__*` — none of which appear in the actual allowlists (`coder.rs:22-34`, `debugger.rs:21-38`, `researcher.rs:21-32`). They only become real if MCP servers are configured (`runner.rs:323-330`). A reader trusting the headers will mis-model the security boundary. Also `coder.rs:4` "Forbidden from genealogy / kinship" is a leftover from an unrelated domain spec.
2. **Vestigial recursion API.** `may_recurse()` (`mod.rs:222`) is never consulted in production; `DelegationRequest::recursion_granted` (`mod.rs:151`) is always `false` at every call site (`manager/loop.rs:725`, `orchestrator/steer.rs:565`, `orchestrator/mod.rs:512`), and the depth gate is explicitly unconditional (`orchestrator/mod.rs:273-289`). Dead surface area that suggests a gating mechanism that no longer exists.
3. **Fail-open validation (correctness risk).** Missing `verdict` defaults to `APPROVED` (`validation.rs:213-215`); 3 nudges without a verdict → approved (`validation.rs:262-267`); loop end without verdict → approved (`validation.rs:366-378`). This contradicts the Validator charter "never inflates pass status" (`validator.rs:5-6`). An unavailable validator model effectively approves everything.
4. **Unused `ValidationOutcome`.** Declared and exported (`validation.rs:9-26`, `agents/mod.rs:17`) but `run_automated_validation` returns `(bool, String)` (`validation.rs:28`) — the richer type is dead. Similarly `run_automated_validation`'s first parameter `_client` is ignored (`validation.rs:28`), a signature smell.
5. **String-typed protocol between runner and UI.** `TuiRenderer::on_event` routes status lines to subagents by prefix-matching the agent tag against free-form status strings (`tui/mod.rs:390-421`), and waiting-state detection keyword-matches `"Running"`, `"thinking"`, `"calling backend"`, `"Arbitrating"` (`tui/mod.rs:352-361`). Any rewording of `emit_status` strings in `runner.rs`/`validation.rs` silently breaks panel routing. A structured status event would remove this fragility.
6. **Duplicated logic.** `is_abort` in `tui/mod.rs:868-876` duplicates `helpers::is_abort_command` (`helpers.rs:131-141`); transcript rehydration is implemented twice (`raw.rs:146-205` vs `tui/mod.rs:776-855`); the `agent-taskid` name construction is re-derived in at least four places (`helpers.rs:199-205`, `tui/mod.rs:437-441`, `runner.rs:317-323`, `helpers.rs:280-287`); and the entire steer-decision interpretation matrix exists twice — async in `spawn_steer_arbitration` (`bridge.rs:35-215`) and sync in `RendererSink::on_pause` (`bridge.rs:423-588`) — a classic drift hazard.
7. **`run_session` is a ~900-line monolith.** `src/ui/session.rs:18-925` mixes recovery, rehydration, steering, parallel and sequential tool dispatch, and plan-completion logic; the 20 ms polling/drain blocks are near-verbatim copies between the parallel path (`session.rs:500-560` region) and the sequential path (`session.rs:726-786` region). Extracting a `pump_ui_channels` helper would remove the largest duplication in the UI layer.
8. **Canned-success fallback.** `run_specialist_llm` returns a fabricated `MISSION COMPLETE` deliverable when the live path is unavailable (`runner.rs:15-39`). Correct for tests, but if config loading fails in production (`runner.rs:55-56`) a specialist silently "succeeds". The test-detection heuristic via `current_exe` containing `/deps/` (`runner.rs:46-54`) is fragile, though `MARMEL_LIVE_TEST` provides an escape hatch.
9. **Case-sensitive marker rewriting.** `assemble_final_deliverable` rewrites only the exact literals `"MISSION COMPLETE"` and `"mission complete"` to `REVOKED` (`runner.rs:270-272`); mixed-case variants (e.g. `Mission Complete`) survive revocation, and the plan layer's parser (`manager/phase.rs`) upper-cases before matching, so the two layers can disagree on edge-case text.
10. **`src/widget.rs` is orphaned.** Not declared in `lib.rs` (`src/lib.rs:3-19`) or `main.rs`, and unreferenced anywhere (zero grep hits for `mod widget` / `crate::widget`), so it is not compiled at all. Either wire it in or delete it; its doc claims reuse "by any renderer" (`widget.rs:7-9`) that never happened.
11. **Per-chunk tokenization on the UI thread.** Every streamed delta is encoded with `tiktoken_rs::cl100k_base_singleton()` for token accounting (`tui/mod.rs:281-285, 297-301, 313-317, 328-332, 344-348`; `events.rs:640-643`). The singleton is mutex-backed; encoding every delta on the render path is measurable overhead under fast streams. Chunk-length estimation would suffice for a display counter.
12. **Rendering with side effects.** `render_chat`/`render_plan`/`render_subagents` mutate scroll state and caches (`render.rs:313-317, 470-476, 566-580`), and `draw` performs disk I/O (`render.rs:151-196`). Throttled and pragmatic, but it couples frame drawing to state transitions and makes future renderer refactors (e.g. offscreen rendering) harder.
13. **String-typed worker statuses.** `"Approved"`, `"Revising (rejected pass {i}/{n})"`, `"Failed (max validator iterations exceeded)"` etc. are magic strings scattered through `runner.rs` (`runner.rs:618, 644-648, 672-675, 815-819`) and consumed only by the active-worker table; an enum would prevent typos and ease testing.
14. **Test coverage asymmetry.** The role structs and marker logic are well unit-tested (`coder.rs:48-59`, `mod.rs:226-336`, `registry.rs:199-247`), and the UI has strong suites (`src/ui/session_tests.rs` — 10 wiremock-backed integration tests including parallel delegations; `src/ui/tui/tests.rs` — 28 unit tests over formatting/wrapping/history). But `runner.rs` and `validation.rs` have **no embedded tests**; they are covered only indirectly by `tests/test_validation_loop.rs`, which must route around the `/deps/` live-call bypass via `MARMEL_LIVE_TEST` (`tests/test_validation_loop.rs:11, 152`).

### 6.3 Tight-coupling map

- `runner.rs` ↔ `orchestrator` (active-worker registry, `emit_status`, `PreemptibleStreamSink`) ↔ `harness` (dispatch, monitor) ↔ `llm` (streaming) — the specialist loop touches every subsystem directly via `crate::` paths rather than through interfaces.
- `session.rs` ↔ `bridge.rs` ↔ `helpers.rs` form a triangle where steering state (`steer_queue`, `steer_abort_requested`, `subagents`) is passed by mutable reference through four layers.
- `tui/mod.rs` knows the string formats of `runner.rs` status lines and the semantics of `DelegationEvent` variants; `render.rs` knows the on-disk plan file layout (`render.rs:151-160` reads `.marmel/execution_plan.md` paths directly from `manager::phase` constants).

---

## 7. Summary Table

| Subsystem | Files | Core mechanism | Key risk |
|---|---|---|---|
| Agent model | `agents/mod.rs`, 5 role files, `orchestrator/registry.rs` | `Specialist` trait + registry allowlists enforced at dispatch | Doc/code drift on tool namespaces; dead `may_recurse` |
| Lifecycle | `agents/runner.rs` | Streaming turn loop with nudges, repetition/budget guards, marker assembly | Canned `MISSION COMPLETE` fallback; string-matched markers |
| Validation | `agents/validator.rs`, `agents/validation.rs` | Per-target audit prompts, `leave_verdict`, bounded re-work | Fail-open on missing verdict |
| UI | `ui/mod.rs`, `session.rs`, `bridge.rs`, `raw.rs`, `tui/*` | `Renderer` trait; event enum; raw vs TUI front-ends | `run_session` monolith; duplicated steer paths; string-routed status |
| Widgets | `widget.rs` | Declarative widget parser | Orphaned — not compiled, unused |
| Debug log | `debug_log.rs` | Opt-in append-only `debug.log` covering LLM/tools/MCP/delegation/validation/plan/steering | None significant; verbose by design |

MISSION COMPLETE (t-analysis-agents-ui)