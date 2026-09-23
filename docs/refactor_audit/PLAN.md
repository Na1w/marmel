# Refactor Execution Plan — Ratified Order

> **Status:** Ratified plan merging the three completed audits. Inputs (do NOT re-audit):
> - `docs/refactor_audit/structure.md` — module map, dependency graph, findings F1–F7, deprecated alias, scratch artifacts.
> - `docs/refactor_audit/duplicates.md` — ranked duplication findings §1–§6.
> - `docs/refactor_audit/architecture.md` — §2 types split + §2.3 facade rule, §3 decomposition proposals, §7 target module tree & ordered execution.
>
> **Task numbering** is shared with `execution_plan.md` (t-b2q1 … t-c5z2) so orchestrator and coders use one ID scheme.
> **Global constraint (moves + re-exports only)** applies to the restructure entries (E3–E8): no logic changes, no signature changes, no dependency-direction fixes. Behavior changes are allowed ONLY where explicitly marked (E11).

## Gate rule (applies to EVERY entry)

After each entry, before the next may start:

1. `cargo build --all-targets` — **green**.
2. `cargo test` — **parity vs `docs/baseline_before_refactor.md`**: 370 pass / exactly 1 pre-existing failure
   (`test_handle_delegate_task_rejection_anchoring`, `tests/test_orchestrator.rs`). No new failures, no deleted tests.
3. Exceptions: E9 (t-c1v9) adds one guard test → expected **371 pass / 1 failure** from then on. E13 (t-c5z2) must keep the
   same test count as the then-current baseline (no test deletion; report counts before/after).
4. Lint: `cargo clippy --all-targets` at baseline (0 warnings) — no new warnings on any entry.

---

## Ordered task list

Entries are strictly sequential. E-number = execution order; **Task ID** = `execution_plan.md` ID.

### E1 — Workspace hygiene — `t-b2q1` — risk: **low**
- **Change:** Delete untracked scratch dirs `example_failure/`, `execution_failure_2/`, `.idea/` from disk. Extend
  `.gitignore` to cover them plus `.kvaser/` and top-level `execution_plan*.md`. Keep `marmel.toml.example` as the
  canonical template; ensure local `marmel.toml`/`marmel.toml.cloud` (machine-specific only) are gitignored — do NOT
  delete the user's active config. `git rm -r --cached` ONLY for currently-tracked files that should be untracked
  (verify with `git ls-files` first; skip if none). No commits.
- **Source:** structure.md §5 (leftover/scratch artifacts).
- **Depends on:** nothing. **Blocks:** nothing.

### E2 — Remove deprecated `agent` alias — `t-b3r5` — risk: **low**
- **Change:** Migrate the 2 live call sites — `src/main.rs` import (~line 4) and `agent::phase::Plan::default()`
  (~line 97) — to `crate::manager` / `marmennill::manager`. Fix stale doc comments in `harness/workspace.rs` (~lines
  15, 19). Delete `#[deprecated] pub use manager as agent;` (lines 6–7) from `src/lib.rs`.
- **Verification:** `grep -rn "as agent" src/` and `grep -rn "crate::agent\|marmennill::agent" src tests` → zero matches.
- **Source:** structure.md §3.
- **Depends on:** nothing (independent of E1; keep E1 first for hygiene). **Blocks:** nothing.

### E3 — Split `src/types.rs` (731 LOC) → `src/types/` — `t-b4s6` — risk: **low**
- **Change (per architecture.md §2):**
  - `src/types/wire.rs` (~150 LOC): `Message` (+impl), `ToolCall` (+`new`), `ToolFunction`, `ChatRequest`,
    `ChatChunk`, `ChunkChoice`, `ChunkDelta`, `ChunkToolCall`, `ChunkToolFunction` + the 2 `ToolCall` tests.
  - `src/types/tools.rs` (~490 LOC): `ToolDef`, `ToolFunctionDef`, all 18 schema builders, `minify_json_schema`,
    `from_mcp`, `default_tools`, `manager_tools` + the 3 `from_mcp` tests.
  - `src/types/mod.rs`: **facade re-exporting EVERY pub item** (§2.3 rule: `crate::types::X` paths must break nowhere;
    22 consumer files keep working unchanged).
- **Note:** `ToolDef::from_mcp` stays in `types/tools.rs` for now — moving it to `mcp` (leak F3) is future work.
- **Source:** architecture.md §2 (overrides the suggested events/plan/deliverables/config buckets — `types.rs`
  contains only OpenAI wire DTOs + `ToolDef` builders).
- **Depends on:** E2 (types.rs is touched by no other entry; E2 first keeps the alias gone before path churn).
  **Blocks:** E5–E8 (harness/orchestrator consumers reference `ToolDef` — `ToolDef` moves into `types/tools.rs` here,
  so the types split MUST precede the harness splits), E9 (constants land in the new `types/tools.rs`).

### E4 — Split `ui/session.rs` (1227) → `ui/session/` and `ui/bridge.rs` (1467) → `ui/bridge/` — `t-b5t2` — risk: **low**
- **Change (per architecture.md §3.1/§3.2):**
  - `ui/session/`: `mod.rs` (run_session entry + loop skeleton + `#[path="tests.rs"]` + re-exports), `recovery.rs`
    (deep-freeze recovery), `turn.rs` (per-turn driver), `tools.rs` (tool dispatch, monitor intervention, delegation
    lifecycle), `input.rs` (input/command handling, steer-queue drain, transcript appends), `tests.rs` (moved from
    `ui/session_tests.rs`).
  - `ui/bridge/`: `mod.rs` (re-exports only), `steer.rs` (SteerArbEvent, SharedSteeringHistory,
    spawn_steer_arbitration), `drain.rs` (drain_steer_arbitration_events(_with_transcript)), `sink.rs`
    (RendererSink + StreamSink impl), `tests.rs` (moved from inline `mod tests`, lines 935–1467).
  - `ui/mod.rs` keeps `pub use session::run_session` so `main.rs` is untouched.
- **Rule:** every `crate::…` path stays byte-identical; do NOT clean up imports while moving (§1.3 rule).
- **Source:** architecture.md §3.1–3.2.
- **Depends on:** E3. **Blocks:** E10 (poll_input unification touches the test doubles that move into
  `session/tests.rs` + `bridge/tests.rs` here), E13 (test doubles relocated here).

### E5 — Split `harness/mod.rs` (1188) → `common.rs` + `plan.rs` + `sleep.rs` — `t-b6u4` (part a) — risk: **low**
- **Change (per architecture.md §3.3):** move `ToolInvocation`, `ToolResult`, `HarnessStats`, `ToolCaller`, `ToolError`
  to `harness/common.rs`; `write_plan(_async/_internal)` + `archive_plan` to `harness/plan.rs`; `handle_sleep` /
  `handle_sleep_async` to `harness/sleep.rs`. `harness/mod.rs` shrinks to globals + workspace/MCP accessors +
  dispatch family + `block_on_safe` + inline tests, and re-exports every moved item so `crate::harness::X` paths are
  unchanged.
- **Explicitly NOT done here:** breaking the harness↔orchestrator cycle (F1) — dispatcher call sites are logic; note
  the cycle in the entry report and leave it intact (architecture.md §1.1, §7.2).
- **Source:** architecture.md §3.3; structure.md F1/F4 (deferred).
- **Depends on:** E3 (types split first — `ToolDef` and friends must already be in `types/`). **Blocks:** E6
  (monitor split references harness types), E9 (constants replace literals in the moved `harness/plan.rs`/`pty.rs`).

### E6 — Split `harness/monitor.rs` (1261) → `harness/monitor/` — `t-b6u4` (part b) — risk: **low**
- **Change (per architecture.md §3.4):** `monitor/mod.rs` (HarnessMonitor, Intervention, ToolCallRecord,
  prune_orphan_tool_messages, `#[path="monitor_tests.rs"]`, re-exports), `xml.rs` (XMLToolRescue + 9 block-parsing
  helpers), `repetition.rs` (ToolRepetitionDetector, RepetitionDetector, semantic-JSON helpers), `code.rs`
  (is_code_pattern/line/word, is_markdown_divider), `monitor_tests.rs` moved from `harness/monitor_tests.rs`.
  `crate::harness::monitor::X` paths preserved by re-exports.
- **Source:** architecture.md §3.4.
- **Depends on:** E5. **Blocks:** E9 (monitor.rs name-comparison literals move into `monitor/xml.rs`/`code.rs` —
  constants must be applied to the post-split files).

### E7 — Decompose `orchestrator/mod.rs` (855) → `delegation.rs` + `delegate.rs` — `t-b6u4` (part c) — risk: **low**
- **Change (per architecture.md §3.5):** `OrchestrationConfig`, `DelegationEvent`, `RecursionDepth`, `Delegation` to
  `orchestrator/delegation.rs`; `handle_delegate_task`, `caller_allows_tool`, `TASK_LINE_RE`, `brief_for_task` to
  `orchestrator/delegate.rs`. `mod.rs` keeps OrchestratorManager + `#[path="tests.rs"]` + re-exports, INCLUDING the
  F5 agents re-exports (deleting them is future work). `crate::orchestrator::X` paths unchanged for `ui`, `harness`
  (F1 sites), `manager`.
- **Source:** architecture.md §3.5; structure.md F5 (deferred).
- **Depends on:** E5/E6 (harness split first — orchestrator consumes `crate::harness::{HarnessStats, ToolError,
  ToolResult}` which must already sit in `harness/common.rs`). **Blocks:** E9 (registry.rs literals), E12
  (fix-loop consolidation touches `handle_delegate_task` neighborhood).

### E8 — Split `agents/validation.rs` (911) → `agents/validation/` — `t-b6u4` (part d) — risk: **low**
- **Change (per architecture.md §3.6):** `validation/mod.rs` (ValidationOutcome, is_leave_verdict_tool,
  parse_verdict_args, run_automated_validation, run_plan_validation, tests, re-exports), `automated.rs`
  (run_automated_validation_inner ~410 LOC), `plan.rs` (run_plan_validation_inner ~265 LOC).
  `crate::agents::validation::X` paths preserved; deep-path consumers (`harness/mod.rs:264, 846`) unchanged;
  `agents/mod.rs` re-export `pub use validation::ValidationOutcome` unchanged.
- **Source:** architecture.md §3.6.
- **Depends on:** E5 (harness split first — validation.rs is a harness consumer; keep it after all `crate::harness::`
  paths are stable). **Blocks:** E12 (fix-loop consolidation operates on `automated.rs`/`plan.rs` inner functions).

### E9 — Centralize tool-name strings — `t-c1v9` — risk: **med**
- **Change (per duplicates.md §1):** replace raw tool-name literals (~120 sites / 249 hits) with `tool_names.rs`
  constants. Worst files: `agents/catalog.rs` (42 — build a shared `base_tool_list()` helper),
  `agents/prompt_builder.rs` (18), `harness/monitor/` (10, post-E6), `agents/validator.rs` (10),
  `orchestrator/registry.rs` (9 + 4 `"terminal__"` prefixes — prefix constant + helper for dynamic
  `"terminal__"+suffix`), `harness/pty.rs` (9). Extend `tool_names.rs` if a name is missing.
- **Guard test:** add ONE test in `tests/` scanning `src/*.rs` (include_str!/std::fs) for known tool-name literals
  not preceded by the constants import — pragmatic assertion over a maintained list, not a fragile whole-tree regex.
  **Gate shifts to 371 pass / 1 failure from this entry onward.**
- **Source:** duplicates.md §1.
- **Depends on:** E3–E8 (all restructure moves done first — apply constants to final file locations, not to files
  that will move again). **Blocks:** E13 (test fixtures should use the same constants).

### E10 — Unify `poll_input` surface — `t-c2w3` — risk: **med**
- **Change (per duplicates.md §2):**
  1. Extract the verbatim TUI drain+flush block (`ui/tui/mod.rs` ~840–871) into one shared helper
     (`drain_and_throttle_flush`); `poll_input` and `read_input` both call it.
  2. Unify the four divergent abort-flag implementations (`raw.rs`, `tui/mod.rs`, `session/tests.rs`
     RecordingRenderer, `bridge/tests.rs` TestRenderer — post-E4 locations) behind one trait default backed by a
     shared `InputState { aborted, user_exit }`, **preserving** blocking-TUI vs non-blocking-raw semantics.
  3. Deduplicate the two byte-identical no-op test doubles (raw.rs pair vs TestRenderer pair).
- **Source:** duplicates.md §2.
- **Depends on:** E4 (the test doubles and the TUI methods must already be in their final `session/`/`bridge/`
  locations). **Blocks:** E13 (RecordingRenderer base is consumed by the test-suite consolidation).

### E11 — Deduplicate SSE/HTTP plumbing + MCP retry — `t-c3x1` — risk: **high**
- **Change (per duplicates.md §3):** extract the shared SSE pump skeleton, watchdog idiom, and client builder from
  `llm/client.rs` + `mcp/http.rs` into a common module (e.g. `src/net/sse.rs` or shared `streaming_common` —
  follow architecture.md). `llm/client.rs` keeps only its SSE payload state machine; `mcp/http.rs` keeps only
  JSON-RPC envelope logic (incl. dedup of its internal `post()`/`send_notification()` boilerplate).
- **⚠ ALLOWED INTENTIONAL BEHAVIOR CHANGE:** give MCP the same retry/backoff policy as the LLM client
  (MAX_ATTEMPTS=3, linear backoff, retry on 503/429/502/504 + transport errors). Document in code comments and in
  the entry report. **Verify no test asserts the old no-retry behavior.**
- **Source:** duplicates.md §3 (rank #3 — HIGH retry gap).
- **Depends on:** E3 (types stable). Independent of E9–E10; keep after E8 so all restructure is frozen.
  **Blocks:** nothing.

### E12 — Consolidate validator/fix-loop (~500 LOC) — `t-c4y7` — risk: **high**
- **Change (per duplicates.md §6b):** extract the shared loop scaffolding duplicated across
  `agents/validation/automated.rs` + `agents/validation/plan.rs` (post-E8) and `agents/runner/execution.rs`
  (`run_specialist` turn loop) into one generic driver (e.g. `run_validator_loop(tag, cfg, brief, prompt,
  abort_is_err)`); wrappers keep only prompt/brief text. **RECONCILE the already-diverged abort semantics
  deliberately — pick the stricter/safer behavior and keep it documented.** Also fold the remaining per-specialist
  constructor boilerplate flagged in §6 (keep role prompts static).
- **⚠ BEHAVIOR-ADJACENT:** the validator feedback-loop fix from `execution_plan.completed.2.md` must stay intact —
  inspect its tests before/after this entry.
- **Source:** duplicates.md §6 (rank #1 by size×risk).
- **Depends on:** E8 (validation split first — consolidation operates on `automated.rs`/`plan.rs`), E7
  (orchestrator delegate split first — `handle_delegate_task` neighborhood stable). **Blocks:** nothing.

### E13 — Test-suite consolidation — `t-c5z2` — risk: **med**
- **Change (per duplicates.md §4):**
  1. Put shared fixtures in `tests/common` (extend existing `tests/common/mod.rs`).
  2. Collapse the 5 copies of `test_manager()` (`orchestrator/tests.rs`, `manager/loop_tests.rs` + alias,
     `ui/session/tests.rs`, `tests/test_dynamic_prompts.rs`, `tests/test_orchestrator.rs`) into ONE canonical
     fixture with the correct `.marmel` plan root.
  3. Deduplicate the byte-identical SSE payload builders (`llm/client_tests.rs::sse_body` vs
     `tests/common::completion_sse`; rewrite the inline literals in `tests/test_specialist_stream.rs`).
- **NO test deletion:** count before and after (`cargo test --no-run` + `grep -c "#\[test\]"` per dir) and report
  both numbers. Expected count = 371 pass / 1 failure (incl. the E9 guard test).
- **Source:** duplicates.md §4.
- **Depends on:** E4 (doubles relocated), E9 (constants in fixtures), E10 (RecordingRenderer base exists).
  **Blocks:** nothing — this is the final Phase 3 entry; Phase 4 gates (t-d1a4/t-d2b8) run after it.

---

## Dependency summary (critical ordering rules)

1. **E3 (types split) before E5–E8 (harness/orchestrator splits)** — `ToolDef` moves into `types/tools.rs` in E3;
   harness/orchestrator consumers must reference the post-split types.
2. **E4 (ui session/bridge split) before E10 (poll_input unification)** — the test doubles and TUI methods must be in
   their final `session/`/`bridge/` locations before the drain/flush extraction and abort-flag unification.
3. **E5 → E6 → E7 → E8** (harness mod → monitor → orchestrator → agents/validation) — bottom-up by dependency so no
   entry moves code another pending entry touches; each keeps `crate::…` paths stable via re-exports.
4. **E9 (tool-name constants) after E3–E8** — apply constants to final file locations, never to files that move again.
5. **E12 (fix-loop consolidation) after E7 + E8** — operates on `validation/automated.rs`, `validation/plan.rs` and
   the stable `handle_delegate_task` neighborhood.
6. **E13 (test consolidation) last in Phase 3** — consumes the relocated doubles (E4), constants (E9), and the
   RecordingRenderer base (E10).
7. E1/E2 are independent hygiene/alias entries and run first; E11 is independent of E9/E10 but runs after E8 so all
   restructure is frozen.

**Deferred (NOT in this refactor — documented, per architecture.md §7.2 / structure.md F1–F7):** breaking the
harness↔orchestrator cycle (F1, needs a `ToolPolicy` trait + injection), F2 manager↔agents, F3 `from_mcp` move,
F4 `*_with_engine` variants, F5 re-export deletion, F6 debug_log→tracing, F7 global-state DI pass, and the
`ui/tui` pair (render.rs 1098 / tests.rs 2240) if the <800 LOC rule is applied crate-wide.

---

## Confirmation — target module tree (architecture.md §7, verbatim-in-essence)

Final `src/` layout after all moves (changes vs. today marked; unmarked files are untouched):

```
src/
├── lib.rs                                  (unchanged; `pub mod types;` now resolves to types/mod.rs)
├── main.rs                                 (unchanged)
├── config.rs                               (unchanged)
├── prompts.rs                              (unchanged; move to agents/ is a future task)
├── tool_names.rs                           (unchanged)
├── debug_log.rs                            (unchanged; tracing migration is a future task)
│
├── types/                                  [NEW DIRECTORY — was types.rs, 731 LOC]
│   ├── mod.rs                              [NEW] facade: pub use of every item (see §2.3 rule)
│   ├── wire.rs                             [NEW] Message, ToolCall, ToolFunction, ChatRequest,
│   │                                         ChatChunk, ChunkChoice, ChunkDelta, ChunkToolCall,
│   │                                         ChunkToolFunction + 2 ToolCall tests
│   └── tools.rs                            [NEW] ToolDef, ToolFunctionDef, all 18 schema builders,
│                                             minify_json_schema, from_mcp, default_tools,
│                                             manager_tools + 3 from_mcp tests
│
├── llm/                                    (unchanged: mod, client, stream, thinking, *_tests)
├── mcp/                                    (unchanged: mod, client, http, http_tests)
│
├── manager/                                (unchanged: mod, context, loop, phase, *_tests)
│
├── agents/
│   ├── mod.rs                              (unchanged: Agent, MissionMarker, DelegationRequest,
│   │                                         IsolatedContext, Deliverable, Specialist)
│   ├── catalog.rs / coder.rs / debugger.rs / generalist.rs / planner.rs /
│   │   researcher.rs / validator.rs / prompt_builder.rs   (unchanged)
│   ├── runner/                             (unchanged: mod, assembly, execution, formatting)
│   └── validation/                         [NEW DIRECTORY — was validation.rs, 911 LOC]
│       ├── mod.rs                          [NEW] ValidationOutcome, is_leave_verdict_tool,
│       │                                     parse_verdict_args, run_automated_validation,
│       │                                     run_plan_validation, tests, re-exports
│       ├── automated.rs                    [NEW] run_automated_validation_inner (~410 LOC)
│       └── plan.rs                         [NEW] run_plan_validation_inner (~265 LOC)
│
├── harness/
│   ├── mod.rs                              [SHRUNK] globals + workspace/MCP accessors + dispatch
│   │                                         family + block_on_safe + inline tests + re-exports
│   ├── common.rs                           [NEW] ToolInvocation, ToolResult, HarnessStats,
│   │                                         ToolCaller, ToolError
│   ├── plan.rs                             [NEW] write_plan(_async/_internal), archive_plan
│   ├── sleep.rs                            [NEW] handle_sleep, handle_sleep_async
│   ├── fs.rs / pty.rs / search.rs / sandbox.rs / workspace.rs / *_tests.rs   (unchanged)
│   └── monitor/                            [NEW DIRECTORY — was monitor.rs, 1261 LOC]
│       ├── mod.rs                          [NEW] HarnessMonitor, Intervention, ToolCallRecord,
│       │                                     prune_orphan_tool_messages,
│       │                                     #[path="monitor_tests.rs"] tests, re-exports
│       ├── xml.rs                          [NEW] XMLToolRescue + 9 block-parsing helpers
│       ├── repetition.rs                   [NEW] ToolRepetitionDetector, RepetitionDetector,
│       │                                     semantic-JSON helpers
│       ├── code.rs                         [NEW] is_code_pattern/line/word, is_markdown_divider
│       └── monitor_tests.rs                [MOVED] (from harness/monitor_tests.rs)
│
├── orchestrator/
│   ├── mod.rs                              [SHRUNK] OrchestratorManager + #[path="tests.rs"]
│   │                                         + re-exports (incl. kept agents re-exports, F5)
│   ├── delegation.rs                       [NEW] OrchestrationConfig, DelegationEvent,
│   │                                         RecursionDepth, Delegation
│   ├── delegate.rs                         [NEW] handle_delegate_task, caller_allows_tool,
│   │                                         TASK_LINE_RE, brief_for_task
│   ├── bus.rs / freeze.rs / preemption.rs / registry.rs / steer.rs /
│   │   steer_extractor.rs / plan_summary.rs / workers.rs / tests.rs / steer_tests.rs
│   │                                         (unchanged)
│
└── ui/
    ├── mod.rs                              (unchanged: Renderer trait, Event, SubagentDetail,
    │                                         restore; `pub use session::run_session` still works)
    ├── bridge/                             [NEW DIRECTORY — was bridge.rs, 1467 LOC]
    │   ├── mod.rs                          [NEW] re-exports only
    │   ├── steer.rs                        [NEW] SteerArbEvent, SharedSteeringHistory,
    │   │                                     spawn_steer_arbitration
    │   ├── drain.rs                        [NEW] drain_steer_arbitration_events(_with_transcript)
    │   ├── sink.rs                         [NEW] RendererSink + StreamSink impl
    │   └── tests.rs                        [MOVED] (was inline mod tests, lines 935–1467)
    ├── session/                            [NEW DIRECTORY — was session.rs, 1227 LOC]
    │   ├── mod.rs                          [NEW] run_session (entry + loop skeleton) +
    │   │                                     #[path="tests.rs"] tests + re-exports
    │   ├── recovery.rs                     [NEW] Deep-Freeze recovery block
    │   ├── turn.rs                         [NEW] per-turn driver (stream + LLM call + errors)
    │   ├── tools.rs                        [NEW] tool-call dispatch, monitor intervention,
    │   │                                     delegation lifecycle
    │   ├── input.rs                        [NEW] input/command handling, steer-queue drain,
    │   │                                     transcript appends
    │   └── tests.rs                        [MOVED] (from ui/session_tests.rs)
    ├── raw.rs / transcript.rs / helpers.rs (unchanged)
    └── tui/                                (unchanged: mod, render, events, formatting, tests)
```

**Post-refactor file-size check (target < ~800 LOC):** largest remaining files are `orchestrator/mod.rs` (~520),
`harness/mod.rs` (~650), `agents/runner/execution.rs` (797 — untouched, already at target), `ui/tui/render.rs`
(1,098) and `ui/tui/tests.rs` (2,240) — the TUI pair is out of scope for the six named files; flag for a future
task if the <800 rule is applied crate-wide.
