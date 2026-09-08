# Marmel — Consolidated Codebase Report

> **Purpose:** executive synthesis of five completed analysis deliverables into a single decision-ready report.
> **Sources (primary inputs):** `docs/architecture.md`, `docs/analysis_orchestrator_manager.md`, `docs/analysis_infrastructure.md`, `docs/analysis_agents_ui.md`, `docs/quality_assessment.md`.
> **Method:** consolidation only — no source files were modified. Claims cite the underlying analysis doc (section) and, where decisive, the audited source file.

---

## 1. Executive Summary

- **What it is.** Marmel (crate `marmennill`, binary `marmel`, v0.6.0) is an autonomous, terminal-native agentic coding assistant that plans, delegates, executes, and validates multi-step engineering tasks against **any OpenAI-compatible LLM backend** (Ollama, vLLM, OpenRouter, local GPU servers) (`docs/architecture.md` §1; `Cargo.toml`).
- **How it is built.** Single-crate Rust (edition 2024, tokio multi-thread runtime, ~369 locked dependencies) organized into eleven modules: `manager`, `agents`, `orchestrator`, `harness`, `llm`, `mcp`, `ui`, plus `config`, `prompts`, `types`, `tool_names` (`docs/architecture.md` §3, §5).
- **Core pattern: fractal Manager + Specialists.** An `OrchestratorManager` decomposes a goal into a disk-backed plan (`.marmel/execution_plan.md`) and delegates atomic tasks to five specialists (coder, researcher, debugger, validator, generalist), each in a strictly isolated context that never sees the Manager's transcript (`docs/analysis_orchestrator_manager.md` §1; `src/agents/mod.rs`).
- **Least privilege is real, not aspirational.** The Manager physically cannot write files or run commands — enforced simultaneously at the prompt, tool-schema (`ToolDef::manager_tools`), and dispatch layers (`src/harness/mod.rs`); the Validator is read-only with a dedicated `leave_verdict` tool (`docs/analysis_agents_ui.md` §1).
- **Disk is the single source of truth.** Plan state, crash journal (Deep-Freeze), transcripts, and logs all live in `.marmel/`; the plan file drives phase transitions, auto check-off, and completion detection, with a tested "no stale-archive resurrection" rule (`docs/analysis_orchestrator_manager.md` §4–5; `src/manager/phase.rs`).
- **Infrastructure is production-grade in the streaming path.** The LLM client layers three watchdogs (300 s first byte / 60 s stall / 1200 s total), retryable-error classification with backoff, resumable mid-flight steering with assistant-prefill continuation, and a thinking-channel demuxer (`docs/analysis_infrastructure.md` §2).
- **Resilience is engineered, not bolted on.** A harness monitor blocks semantic tool-call loops and degenerate text repetition (with careful false-positive suppression for code), rescues XML-formatted tool calls, and the PTY layer guarantees process-group SIGKILL teardown (`docs/analysis_infrastructure.md` §1.5, §1.3).
- **Automated validation closes the loop.** Deliverables are audited by role-matched validator prompts on a dedicated (optionally different) model at temperature 0, with bounded re-work (`max_validator_iterations`, default 5) (`docs/analysis_agents_ui.md` §3).
- **Test discipline is unusually strong** — 22 test files (10 integration, 12 unit), `wiremock`-backed, requirement-traceable (`REQ-*` tags), hermetic via a deterministic specialist worker — but coverage is asymmetric: sandbox enforcement, TUI rendering, MCP transport, and the runner/validation modules are under-tested (`docs/quality_assessment.md` §1–3).
- **Overall assessment: high-quality, coherent architecture with a concentrated risk tail.** The dominant weaknesses are security-posture related (fail-open Landlock sandbox, zero OS confinement on macOS/Windows), a fail-open validation loop, a canned-success fallback in the specialist runner, and a handful of concurrency windows and blocking-bridge hazards. None are latent crashes; all are fixable without architectural change (`docs/analysis_infrastructure.md` §6.2; `docs/analysis_agents_ui.md` §6.2; `docs/analysis_orchestrator_manager.md` §6.2).

---

## 2. System Architecture at a Glance

```
                    ┌────────────────────────────────────────────────┐
   user input ────▶ │  UI layer (src/ui/)                            │
                    │  run_session conductor → Renderer trait        │
                    │  ├─ raw.rs   (headless, pipe-friendly)         │
                    │  └─ tui/     (3-panel Ratatui: chat/plan/agents)│
                    └───────┬───────────────────────────▲────────────┘
                            │ events/status/steer        │ streamed deltas
                            ▼                            │ (StreamSink)
                    ┌────────────────────────────────────────────────┐
                    │  Orchestrator (src/orchestrator/)              │
                    │  OrchestratorManager · SpecialistRegistry      │
                    │  bus (events + cancellation) · workers (RAII)  │
                    │  steer arbitrator · preemption · Deep-Freeze   │
                    └───────┬───────────────────────────▲────────────┘
                            │ delegate_task              │ deliverable
                            ▼                            │ (+ MissionMarker)
                    ┌────────────────────────────────────────────────┐
                    │  Manager core (src/manager/)                   │
                    │  turn state machine · plan/phase (.marmel/)    │
                    │  context engine (80% advisory / 90% compaction │
                    │  → 70% / rebirth, pinned prefix)               │
                    └────────────────────────────────────────────────┘
                            │ IsolatedContext (role prompt + brief + snippets)
                            ▼
                    ┌────────────────────────────────────────────────┐
                    │  Agents (src/agents/)                          │
                    │  coder · researcher · debugger · validator ·   │
                    │  generalist — runner turn loop + validation    │
                    └───────┬───────────────────────────▲────────────┘
                            │ tool calls                 │ results
                            ▼                            │
                    ┌────────────────────────────────────────────────┐
                    │  Harness (src/harness/) — single choke point   │
                    │  dispatch_for + role allowlists (RBAC)         │
                    │  fs (path confinement) · pty (process groups,  │
                    │  ulimit, timeouts) · sandbox (Landlock, Linux) │
                    │  monitor (repetition/XML rescue) · search      │
                    └───────┬───────────────────────────▲────────────┘
                            │ SSE chat/completions       │ chunks
                            ▼                            │
                    ┌───────────────────────┐   ┌──────────────────────┐
                    │  LLM (src/llm/)       │   │  MCP (src/mcp/)      │
                    │  ChatClient · stream  │   │  stdio + HTTP/SSE    │
                    │  · thinking demux     │   │  JSON-RPC 2.0,       │
                    └───────────────────────┘   │  server__tool names  │
                                                └──────────────────────┘
```

**Interaction summary.** The user talks only to the UI conductor (`src/ui/session.rs`), which drives the Manager turn loop. The Manager writes the plan to disk and delegates tasks; the orchestrator resolves roles against the registry, enforces the recursion-depth gate, snapshots in-flight delegations to the Deep-Freeze journal, and runs specialists in isolated contexts. Specialist tool calls flow through the harness dispatcher (role-gated, path-confined, output-truncated, repetition-monitored), which prefers MCP tools (qualified `server__tool` names) over built-ins and bridges to the LLM client for streaming. Mid-flight user steering is arbitrated by an LLM call that preempts conflicting model streams, then resumes (via assistant-prefill continuation) or aborts them. Validation runs as a separate read-only specialist pass before a deliverable may check off its plan task (`docs/architecture.md` §5.2; `docs/analysis_orchestrator_manager.md` §1, §3; `docs/analysis_infrastructure.md` §1.1, §3.4; `docs/analysis_agents_ui.md` §4).

---

## 3. Key Strengths

1. **Genuine context isolation.** Specialists receive only role prompt + brief + bounded snippets (`IsolatedContext`, `src/agents/mod.rs:156`); the factory (`ContextEngineFactory`) makes it impossible to seed a specialist with Manager history, verified by tests (`docs/analysis_orchestrator_manager.md` §4.4; `docs/analysis_agents_ui.md` §6.1).
2. **Single dispatch choke point with role-based least privilege.** Every tool call flows through `dispatch_for_with_engine` (`src/harness/mod.rs:435`); the Manager cannot mutate or execute, specialists are registry-gated, and a dedicated test fails the build if worker tool namespaces ever diverge from the registry (`docs/analysis_infrastructure.md` §6.1; `docs/analysis_agents_ui.md` §6.1).
3. **Defense-in-depth execution isolation.** Layered: path confinement (`resolve_safe_path`, `src/harness/fs.rs:37`) → PTY process-group SIGKILL teardown + `ulimit -f` + timeouts (`src/harness/pty.rs`) → Landlock kernel sandbox on Linux via re-exec (`src/harness/sandbox.rs`, `src/main.rs:21-44`) → output sanitization/truncation → behavioral loop-breaking (`docs/analysis_infrastructure.md` §1.8, §6.1).
4. **Production-grade LLM streaming.** Three watchdog layers, retryable-error classification with linear backoff (wiremock-tested 503→429→200 ladder), cooperative abort, and resumable steering with thinking-preserving continuation (`docs/analysis_infrastructure.md` §2, §6.1).
5. **Resilience monitor with false-positive engineering.** Semantic (key-order-insensitive) tool-call repetition detection with pagination exemptions, and text-loop detection that suppresses code-shaped patterns — the hard part done carefully and pinned by tests (`docs/analysis_infrastructure.md` §1.5, §6.1).
6. **Disk-backed plan with double-gated check-off.** A task is marked complete only when both the deliverable's `MissionMarker` and a re-parse of its content agree, defeating stale completion tokens; the plan file is observable, crash-consistent, and immune to archive resurrection (`docs/analysis_orchestrator_manager.md` §6.1, §5).
7. **Deep-Freeze crash recovery done right.** Freeze-before-run, worker-id-scoped clear-after-run prevents concurrent checkpoint stomping; corrupt journals degrade gracefully instead of crashing (`docs/analysis_orchestrator_manager.md` §3.3, §6.1).
8. **Context engine with pinned prefix and tiered compaction.** 80% rebirth advisory → 90% compaction to 70% → retry escalation → full rebirth, always preserving `messages[0]`/`[1]` for KV-cache efficiency (`docs/analysis_orchestrator_manager.md` §4.4).
9. **Clean renderer abstraction and performance-aware TUI.** One `Renderer` trait drives both raw and TUI front-ends; the TUI uses incremental wrapped-line caching, mtime-throttled plan reads, and ~16 ms render cadence (`docs/analysis_agents_ui.md` §4, §6.1).
10. **Excellent observability.** Opt-in `--debug` captures the entire I/O surface (LLM traffic, tool calls, delegations, verdicts, plan mutations, MCP exchanges, steering pauses) in one append-only `debug.log` (`src/debug_log.rs`; `docs/analysis_agents_ui.md` §5, §6.1).
11. **UTF-8 correctness throughout.** Char-boundary-safe truncation, char-based pagination, and emoji-safe partial-tag handling in the thinking demuxer (`docs/analysis_infrastructure.md` §6.1).
12. **Requirement traceability and hermetic tests.** `REQ-*` tags throughout code and tests; a deterministic specialist worker makes end-to-end orchestrator tests run without a live backend (`docs/analysis_orchestrator_manager.md` §6.6; `docs/quality_assessment.md` §2).

---

## 4. Key Risks & Weaknesses (ranked by severity)

### High

| # | Risk | Evidence |
|---|---|---|
| H1 | **Fail-open sandbox.** Landlock failure (unsupported kernel or `restrict_self` error) logs a warning and continues unsandboxed; **macOS and Windows get no OS-level confinement at all**; the parent marmel process is never sandboxed; network egress is unrestricted in every mode. On the primary dev platform (macOS), `run_command` isolation reduces to cwd + `ulimit -f` + timeout. | `src/harness/sandbox.rs:43-49,112-120`; `docs/analysis_infrastructure.md` §4.1, §6.2; `docs/quality_assessment.md` R1 |
| H2 | **Fail-open validation loop.** Every path where the validator fails to emit a verdict approves the deliverable: missing `verdict` argument defaults to `APPROVED`, 3 nudges exhausted → approved, loop end without verdict → approved. An unavailable validator model silently inflates pass status, contradicting the Validator charter ("never inflates pass status"). | `src/agents/validation.rs:213-215,262-267,366-378`; `docs/analysis_agents_ui.md` §3.5, §6.2 |
| H3 | **Canned-success fallback in production.** `run_specialist_llm` returns a fabricated `MISSION COMPLETE` deliverable when the live path is unavailable; if config loading fails outside tests, a specialist silently "succeeds". | `src/agents/runner.rs:15-39,55-58`; `docs/analysis_agents_ui.md` §6.2 |

### Medium

| # | Risk | Evidence |
|---|---|---|
| M1 | **Concurrency windows around the plan.** (a) `run_executing` reads pending tasks outside any lock then spawns delegations — two concurrent loops (or a steer subtask) can double-delegate the same task; (b) `check_off` flips the *first line containing the id as a substring* (`t-01` vs `t-010` can flip the wrong line); (c) the abort-flag clear/re-arm dance in `drain_signals` can lose a mid-flight abort signal. | `src/manager/loop.rs:699-776,281-299`; `src/manager/phase.rs:462-476`; `docs/analysis_orchestrator_manager.md` §6.2 |
| M2 | **Blocking bridges into async contexts.** MCP dispatch and all five PTY tool handlers use `block_in_place` + `block_on` **without** the runtime-flavor guard that `handle_sleep` carefully implements — they panic on current-thread runtimes (library/test callers). | `src/harness/mod.rs:316-318,489-493,568-572`; `src/harness/pty.rs:557-561`; `docs/analysis_infrastructure.md` §6.2, §6.3.1 |
| M3 | **Watchdog gaps.** The 600 s turn watchdog is advisory (checked only at phase boundaries — a long `run_command` cannot be interrupted); `ManagerLoop` has **no watchdog at all**: one delegation that never returns blocks the round indefinitely (cooperative cancellation only). | `src/manager/loop.rs:309-314,695-779`; `docs/analysis_orchestrator_manager.md` §6.5 |
| M4 | **Freeze/recovery robustness gaps.** If the journal snapshot write fails, delegation proceeds unfrozen (crash loses the checkpoint — the "fail loudly" intent is not realized); `recover_frozen` recovers only the **first** frozen snapshot per boot; the fallback journal record fabricates a `(Coder, None)` identity, misleading forensics. | `src/orchestrator/mod.rs:315-323,397-399`; `src/orchestrator/freeze.rs:156-159`; `docs/analysis_orchestrator_manager.md` §6.3, §6.5 |
| M5 | **Sandbox grant surface.** `~/.cargo` and `~/.cache` are writable by design (cache poisoning/exfiltration), `/etc` is readable, `/proc` grants host-process introspection; spawned shells inherit the full parent environment (secrets visible to every command). | `src/harness/sandbox.rs:69-107`; `docs/analysis_infrastructure.md` §4.1, §4.3 |
| M6 | **TOCTOU in path confinement.** Existence check, canonicalization, and I/O are separate operations with no `O_NOFOLLOW`-style hardening; a symlink swapped between check and use can redirect I/O. The temp-dir allowance widens the writable surface beyond the workspace. | `src/harness/fs.rs:37-89,111,140,180`; `docs/analysis_infrastructure.md` §4.2 |
| M7 | **Retry duplicates emitted deltas.** `chat_stream` retries the whole request after mid-stream failure; deltas already pushed to the UI/demuxer are re-emitted, duplicating transcript prefixes and potentially triggering false repetition interventions. | `src/llm/client.rs:155-176`; `docs/analysis_infrastructure.md` §6.2 |
| M8 | **MCP precedence over built-ins.** A configured MCP server whose qualified name collides with a built-in intercepts the call; MCP servers are trusted with arbitrary arguments and run unsandboxed. | `src/harness/mod.rs:313-325`; `docs/analysis_infrastructure.md` §4.4, §6.2 |
| M9 | **Unbounded memory growth.** Interactive PTY `SharedBuffer` never trims consumed output (long-lived sessions accumulate full byte history); `delegation_events` grows without bound if the UI never drains it. | `src/harness/pty.rs:259-265`; `src/orchestrator/mod.rs:187`; `docs/analysis_infrastructure.md` §6.2; `docs/analysis_orchestrator_manager.md` §6.2 |
| M10 | **Panic risk in path resolution.** `unwrap()` in `resolve_safe_path` can crash on unexpected filesystem states. | `src/harness/fs.rs`; `docs/quality_assessment.md` R3 |

### Low

| # | Risk | Evidence |
|---|---|---|
| L1 | **Dead code.** `src/widget.rs` is an orphaned declarative-widget parser — not declared in `lib.rs`/`main.rs`, referenced nowhere, not even compiled; `Specialist::may_recurse()` and `DelegationRequest::recursion_granted` are vestigial (never consulted in production); `ValidationOutcome` is declared but unused; `map_path` survives only in tests. | `src/widget.rs`; `src/agents/mod.rs:151,222`; `docs/architecture.md` §10.1; `docs/analysis_agents_ui.md` §6.2; `docs/analysis_infrastructure.md` §6.4 |
| L2 | **Doc/code drift.** Role doc-headers promise MCP namespaces (`kiwix__*`, `pdf__*`, …) that the code does not grant; monitor docs say "1000-character buffer" while the constant is 16384; README says 0.5.0 vs manifest 0.6.0; duplicated `#[cfg(test)]` attributes and doc lines. | `src/agents/coder.rs:4-6`; `src/harness/monitor.rs:12,30`; `docs/analysis_agents_ui.md` §6.2; `docs/analysis_infrastructure.md` §6.4; `docs/architecture.md` §10.2 |
| L3 | **String-typed protocols.** TUI routes status lines by prefix-matching free-form strings and keyword-matching waiting states — rewording a status string silently breaks panel routing; worker statuses and `SteerDecision.decision` are magic strings rather than enums. | `src/ui/tui/mod.rs:352-361,390-421`; `src/orchestrator/steer.rs:52`; `docs/analysis_agents_ui.md` §6.2 |
| L4 | **Duplication hotspots.** The steer-decision interpretation matrix exists twice (async + sync paths in `bridge.rs`); transcript rehydration twice; `run_session` is a ~900-line monolith with near-verbatim polling blocks; five slightly different task-id regexes across `phase.rs`, `loop.rs`, `orchestrator/mod.rs`, `plan_summary.rs`. | `src/ui/bridge.rs:35-215,423-588`; `src/ui/session.rs:18-925`; `docs/analysis_agents_ui.md` §6.2; `docs/analysis_orchestrator_manager.md` §6.4 |
| L5 | **Performance smells.** Per-chunk `tiktoken` encoding on the UI render path; `read_file` re-reads the whole file per page (quadratic sequential pagination); single-threaded `grep_search` loading whole files; fixed 300 ms sleeps as PTY synchronization. | `src/ui/tui/mod.rs:281-348`; `src/harness/fs.rs:111-121`; `src/harness/search.rs:41-56`; `docs/analysis_agents_ui.md` §6.2; `docs/analysis_infrastructure.md` §6.4 |
| L6 | **Test flakiness & complexity risk.** `std::thread::sleep` in integration tests may destabilize CI; TUI state machine is complex with only helper-level tests. | `tests/test_harness.rs`; `docs/quality_assessment.md` R2, R4 |
| L7 | **Misc.** Hand-rolled streaming JSON extractor in the steer arbitrator (lone surrogates silently dropped); `glob_to_regex` swallows invalid patterns into match-nothing; silent `RwLock` poisoning tolerance can disable MCP dispatch; mixed-case `Mission Complete` variants survive validator revocation. | `src/orchestrator/steer.rs:78-192`; `src/harness/search.rs:123-125`; `src/agents/runner.rs:270-272`; `docs/analysis_orchestrator_manager.md` §6.4; `docs/analysis_infrastructure.md` §6.4; `docs/analysis_agents_ui.md` §6.2 |

---

## 5. Test & Quality Posture

**Inventory** (`docs/quality_assessment.md` §1): 22 test files — 10 integration (`tests/`, `wiremock`-backed) and 12 unit modules (embedded `*_tests.rs` companions). Subsystems are exercised hermetically via deterministic canned deliverables, so orchestrator end-to-end tests need no live backend.

| Dimension | Finding |
|---|---|
| **Well-tested** | LLM streaming/thinking (retry ladder, demux, multibyte), context engine (compaction/rebirth/prefix pinning), orchestrator delegation (depth gate, check-off gating incl. adversarial stale markers), role gating (registry↔worker consistency), filesystem tools (replace uniqueness, path-escape attempts) |
| **Under-tested** | Sandbox enforcement (no negative tests that forbidden paths are actually blocked), TUI rendering (helper tests only, no render-loop integration), MCP transport (envelope logic only, no live SSE test), preemption/freeze-recovery integration, and **`runner.rs`/`validation.rs` have no embedded tests** (covered only indirectly via `MARMEL_LIVE_TEST`) |
| **Determinism & flakiness** | High determinism in units; `std::thread::sleep` and fixed-duration PTY waits in integration tests are CI flakiness risks |
| **Code hygiene** | `unsafe` confined to PTY process-group kills (well-scoped, `Drop`-deterministic, edition-2024 constraints acknowledged); `unwrap()`/`expect()` occasionally in production (`resolve_safe_path`); consistent `anyhow` error handling; no significant TODO/FIXME debt |
| **Traceability** | `REQ-*` requirement tags across code and tests; registry consistency enforced by a build-failing test |

**Net judgment:** quality culture is well above average for an agent codebase; the gaps cluster exactly where the highest-severity risks live (sandbox, validation, runner lifecycle) — closing those three test gaps would disproportionately de-risk the system.

---

## 6. Recommendations — Prioritized Roadmap

### Quick wins (low effort, high leverage)

1. **Fail-closed validation** (H2): default missing/absent verdicts to `REJECTED` or introduce a distinct `Inconclusive` outcome; wire up the already-declared `ValidationOutcome` enum instead of the `(bool, String)` tuple. (`src/agents/validation.rs`)
2. **Kill the canned-success fallback in production** (H3): distinguish test mode from config-load failure; a config failure must produce `FAILED`, never `MISSION COMPLETE`. (`src/agents/runner.rs:15-58`)
3. **Sandbox strict mode + negative tests** (H1, quality P0): make sandbox failure configurable (strict = abort the tool call, permissive = warn) and add tests that attempt to read forbidden paths from inside the sandbox. (`src/harness/sandbox.rs`)
4. **Runtime-flavor guard** for MCP/PTY `block_in_place` bridges (M2): reuse the `handle_sleep` guard pattern or convert handlers to async. (`src/harness/mod.rs`, `src/harness/pty.rs`)
5. **Replace `unwrap()` in `resolve_safe_path`** with typed errors (M10); replace `std::thread::sleep` with `tokio::time::sleep` in tests (L6). (`src/harness/fs.rs`, `tests/test_harness.rs`)
6. **Doc/code drift sweep** (L2): fix monitor buffer docs, role-header namespace claims, README version, duplicated `#[cfg(test)]` attributes; add the missing `macOS: no OS sandbox` caveat to security docs. (`src/harness/monitor.rs`, `src/agents/*.rs`, `README.md`)
7. **Delete or wire dead code** (L1): remove `src/widget.rs` or declare and integrate it; drop `may_recurse`/`recursion_granted` or actually consult them; use or remove `ValidationOutcome` and `map_path`.
8. **macOS sandbox parity** (H1): evaluate a Seatbelt (`sandbox-exec`) profile for the re-exec path to close the primary-platform gap.

### Structural improvements (planned work)

1. **Concurrency hardening around the plan** (M1): hold a lock (or compare-and-swap on file content) across the read-pending→dispatch→check-off window; anchor `check_off` matching to checkbox-line regexes rather than substring containment; document the abort-flag memory-model guarantee.
2. **Watchdog coverage** (M3): add a delegation-level timeout to `ManagerLoop`; make the turn watchdog able to interrupt long `spawn_blocking` writes (e.g. cooperative cancellation checks inside `run_command` polling).
3. **Freeze/recovery completeness** (M4): implement `recover_frozen_all`; on snapshot-write failure either fail the delegation loudly or mark it unfrozen in the journal; remove the fabricated `(Coder, None)` fallback record.
4. **Structured event protocol** (L3): replace string-prefix status routing with a typed `StatusEvent` enum; convert worker statuses and `SteerDecision.decision` to enums (serde-tagged).
5. **Deduplicate logic** (L4): one shared task-id parser module; single steer-decision interpretation function used by both async and sync paths; extract a `pump_ui_channels` helper from `run_session`; consider splitting the ~900-line session conductor.
6. **Streaming correctness** (M7): buffer deltas per attempt and forward only on successful stream completion (or tag retried streams so the demuxer resets).
7. **Memory & performance** (M9, L5): trim `SharedBuffer` on read or cap total bytes; cap `delegation_events`; estimate UI token counts instead of per-chunk tiktoken encoding; `Seek`-based `read_file` pagination; parallel walker for `grep_search`.
8. **Sandbox surface reduction** (M5, M6): scrub inherited environment variables in spawned shells; reconsider `/etc` readability; add `O_NOFOLLOW`-style openat hardening or document the TOCTOU residual risk; evaluate Landlock ABI V2/V3 for rename/truncate rights.
9. **Test-gap closure**: sandbox negative tests (see quick win 3), a headless TUI render-loop test, an MCP live-SSE integration test, and embedded tests for `runner.rs`/`validation.rs`; preemption and freeze-recovery integration scenarios.
10. **Sync-delegation bridge cleanup**: extract the `handle_delegate_task` thread-scope + `block_on` + `catch_unwind` sandwich into a documented, tested helper; stop constructing throwaway managers (and stats) per call. (`src/orchestrator/mod.rs:624-663`)

---

## 7. Appendix — Index of Source Reports

| # | Report | One-line description |
|---|---|---|
| 1 | `docs/architecture.md` | Read-only architecture survey: project purpose, tech stack, entry-point bootstrap, configuration model, module topology, prompt system, domain types, `.marmel/` runtime state, and testing layout. |
| 2 | `docs/analysis_orchestrator_manager.md` | Deep-dive on `src/orchestrator/*` and `src/manager/*`: delegation flow, registry, concurrency model, steering/preemption, Deep-Freeze, plan lifecycle, context engine, races, smells, and subsystem test coverage. |
| 3 | `docs/analysis_infrastructure.md` | Deep-dive on `src/harness/*`, `src/llm/*`, `src/mcp/*`: tool dispatch and RBAC, filesystem/PTY/sandbox/monitor/search internals, streaming client with watchdogs/retries, MCP transports, security posture, and unsafe/blocking-code audit. |
| 4 | `docs/analysis_agents_ui.md` | Deep-dive on `src/agents/*`, `src/ui/*`, `src/widget.rs`, `src/debug_log.rs`: the five specialist roles, runner lifecycle, validation loop, UI layering (raw/TUI/bridge/session), debug logging, and coupling map. |
| 5 | `docs/quality_assessment.md` | Quality audit: test inventory (22 files), strategy and flakiness risks, coverage-gap analysis, code-quality signals (panics/unsafe/hygiene), a four-item risk matrix, and prioritized recommendations. |

---

*Consolidated report generated from the five analysis deliverables listed above; all line references were inherited from those reports and refer to the workspace state at analysis time. No source files were modified.*