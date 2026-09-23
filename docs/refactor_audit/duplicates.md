# Marmennill — Duplication Audit

Audit of source duplication in `marmel` (v0.9.0). Findings ranked by **size × risk** below; detailed sections follow.
Audit complete (v0.9.0).

## Ranked Summary

Ranked by **(duplicated LOC) × (risk)** — largest first. Full details in §1–§6 below.

| Rank | Finding (§) | Location(s) | ~Dup LOC | Risk | Size×Risk weight | Consolidation approach (one-liner) |
|------|-------------|-------------|---------:|------|------------------|------------------------------------|
| 1 | Fix-loop scaffolding: `run_automated_validation_inner` ≈ copy of `run_plan_validation_inner`; both also share the turn loop with specialist execution (§6b) | `src/agents/validation.rs:149 & :572`; `src/agents/runner/execution.rs:107` | ~500 | HIGH | 5 (top) | One generic `run_validator_loop(tag, cfg, brief, prompt, abort_is_err)` driver; wrappers keep only prompt/brief text |
| 2 | Tool-name raw literals bypassing `tool_names.rs`; per-agent list blocks in catalog; name comparisons in monitor/validator (§1) | 25+ files under src/, worst: `src/agents/catalog.rs` (42), `src/harness/monitor.rs`, `src/agents/validator.rs`, `src/orchestrator/registry.rs` | ~120 sites (≈250 lines touched) | HIGH | 4 | Replace with `TOOL_*` constants; shared `base_tool_list()` builder for catalog entries |
| 3 | SSE pump / watchdogs / retry / HTTP client duplication; MCP side has **no retries at all** (§3) | `src/llm/client.rs:159–440` vs `src/mcp/http.rs:119–270` (+ internal post/notification dup in http.rs) | ~120–150 | HIGH (retry gap) / MED (plumbing) | 4 | `crate::net`: shared SSE-with-watchdog pump, generic `retry_with_backoff(Retryable)`, shared client builder; MCP gets a real retry policy |
| 4 | Test harness copies: `test_manager` ×5, SSE payload builder ×2–3, mock `Renderer` doubles ×3 with divergent queue semantics (§4) | `src/orchestrator/tests.rs:5`, `src/manager/loop_tests.rs:260`, `src/ui/session_tests.rs:203`, `tests/test_{dynamic_prompts,orchestrator}.rs`, `src/llm/client_tests.rs:6` vs `tests/common/mod.rs`, 3 test doubles | ~120–150 | MED | 3 | Crate-level `#[cfg(test-support)] pub mod testing`: one factory, one SSE builder, one composable `RecordingRenderer` base |
| 5 | `poll_input`/`read_input` + abort-flag boilerplate across 4 renderers (TUI read/poll verbatim copy) (§2) | `src/ui/tui/mod.rs:840–871`, `src/ui/raw.rs:127–148`, `src/ui/session_tests.rs:187–195`, `src/ui/bridge.rs:963–974` | ~60 | MED | 2.5 | Extract TUI `drain_and_throttle_flush()`; trait-default no-ops backed by shared `InputState{aborted,user_exit}` |
| 6 | `is_test_runner` exe-path sniffing ×3 (same file twice) + scattered `.marmel` path joins (§5) | `src/agents/runner/mod.rs:42 & :65`, `src/agents/prompt_builder.rs:214`; 5 join sites | ~35 | MED (silent live/canned switch) | 2 | Single `crate::env::is_test_runner()`; `phase::plan_path()/archive_dir()` accessors |
| 7 | Specialist-file embedded tests (~6-line asserts w/ raw names ×5–6) (§6a) | `src/agents/{coder,debugger,planner,researcher,validator}.rs` test mods | ~35 | LOW | 1.5 | One parameterized namespace test in `agents/mod.rs`; doc-comment namespace lists removed (code is the source of truth) |

**Total estimated dedup savings: ≈ 900–1,100 LOC** removed (conservative; §6b and §3 dominate), plus elimination of two latent behavioral drifts (MCP no-retry; validation abort semantics).
## 1. Tool-name string literals outside `src/tool_names.rs`

The crate centralizes tool names in `src/tool_names.rs` (constants `TOOL_*` / `TERMINAL_*`, lines 12–61), but raw string literals still appear in **39 non-test files under src/** and **10 test files**. Grep of the 17 base tool names (`read_file`, `write_file`, `replace`, `run_command`, `grep_search`, `glob`, `create_plan`, `archive_current_plan`, `rebirth`, `pty_spawn/write/read/close/list`, `leave_verdict`, `sleep`, `delegate_task`) found **249 raw-literal hits** project-wide (including `"terminal__…"` prefixed variants in registry/config, see below).

### Hit count per file (non-test, src/)

| File | Raw hits | Notes |
|------|---------:|-------|
| `src/agents/catalog.rs` | 42 | Per-agent tool-lists built from `.to_string()` literals (lines ~129–135 repeat `"read_file".to_string(), "write_file".to_string(), …` block for every specialist) |
| `src/agents/prompt_builder.rs` | 18 | Tool names hard-coded in prompt templates/gating text |
| `src/harness/monitor.rs` | 10 | XML/tool-call parsing matches `name == "write_file"` etc. (lines 300–320) — real comparison logic, highest risk of silent breakage on rename |
| `src/agents/validator.rs` | 10 | Per-tool branching on raw names |
| `src/orchestrator/registry.rs` | 9 (+4 `"terminal__"` prefixed) | Tool dispatch registry keyed by raw strings |
| `src/harness/pty.rs` | 9 | PTY tool name matching |
| `src/harness/search.rs` | 5 | Raw-name fallback search |
| `src/types.rs` | 4 | Default tool lists |
| `src/ui/session.rs`, `src/ui/helpers.rs` | 3 each | UI rendering labels keyed off names |
| `src/debug_log.rs` | 3 | Log filtering by raw name |
| `src/agents/validation.rs` | 3 (+1 prefixed) | Fix-loop tool gating |
| `src/ui/tui/tests.rs`, `src/ui/transcript.rs`, `src/orchestrator/steer.rs`, `src/harness/fs.rs`, `src/config.rs` (2 + 4 prefixed), `src/agents/{coder,debugger,planner,researcher}.rs` | 1–3 each | Scattered literals |

Tests using raw names (lower priority): `tests/test_validation_loop.rs` (35), `test_agent.rs` (30), `test_role_gating.rs` (23), `test_dynamic_prompts.rs` (8), `test_harness.rs` (6).

**~Duplicated LOC:** ≈ 120 distinct literal sites in src/ (each 1–3 lines); not a line-for-line copy but the exact duplication the constants exist to eliminate.
**Risk: HIGH** — renaming a tool requires touching 25+ files; `monitor.rs` / `validator.rs` comparisons can silently miss calls if names drift.
**Consolidation:** Replace literals with `tool_names::*` constants (mechanical `grep -L`-verifiable refactor); for per-agent lists in `catalog.rs`, build a shared `fn base_tool_list() -> Vec<String>` helper using the constants so each agent entry stays one line.


## 2. `poll_input` / `read_input` implementations in `src/ui/`

The audit's claim of "5+ near-duplicates" holds: the `Renderer` trait (`src/ui/mod.rs:78–79`) forces **every** renderer to implement `poll_input` + `read_input`, and there are **4 non-trivial impls plus 2 identical no-op pairs in test doubles**.

| # | Location | `poll_input` | `read_input` | What differs |
|---|----------|--------------|--------------|--------------|
| 1 | `src/ui/tui/mod.rs:840` / `:850` | Drains events via `handle_events(false)`, 25 ms render-throttle flush, then `rx.try_recv()` | Blocking loop: abort check → `handle_events(true)` → `rx.try_recv()`, same 25 ms throttle flush (lines 856–871) | The real TUI. `read_input` is a strict superset of `poll_input`: **the event-drain + flush-throttle block is copy-pasted verbatim** between the two methods (~12 lines). |
| 2 | `src/ui/raw.rs:127` / `:133` | `None` | `None` | Headless no-op pair (correct, but see #4/#5 below). |
| 3 | `src/ui/session_tests.rs:187` / `:190` (`RecordingRenderer`) | `input_queue.pop_front()` | `None` | Test double. Note the *inconsistency*: queued input is only drained by `poll_input`, so a session calling `read_input` sees nothing — two different renderers encode opposite queue semantics. |
| 4 | `src/ui/bridge.rs:963` / `:966` (`TestRenderer`) | `None` | `None` | Test double, **byte-identical no-op pair to raw.rs** (minus docs). |
| 5 | (implicit) other trait impls default only via provided methods; `request_abort`/`aborted`/`clear_abort` are additionally copy-pasted: `raw.rs:136–148`, `tui/mod.rs:873–897`, `session_tests.rs:192–195`, `bridge.rs:968–974` — four variants of the same 2-field flag dance (raw & bridge versions are identical). | | | Shared flag boilerplate. |

**~Duplicated LOC:** ~60 lines (two TUI-internal copies of the drain/flush block ≈12; four abort-flag impls ≈30; two identical no-op pairs ≈8).
**Risk: MEDIUM** — `read_input` vs `poll_input` semantics diverged silently between real renderers and test doubles (test double never returns queued lines to `read_input`), so tests exercise a code path the production session loop may take differently.
**Consolidation:**
1. Extract the TUI drain/flush block into `fn drain_and_throttle_flush(&mut self) -> bool`; have `poll_input` call it once and `read_input` call it in its loop (kills the verbatim ~12-line copy).
2. Give the trait *default* no-op implementations for `poll_input`/`read_input`/`request_abort`/`aborted`/`clear_abort` backed by a small shared `InputState { aborted, user_exit }` struct that renderers compose; deletes all four flag-copy impls and both identical test doubles.


## 3. Stream/SSE + HTTP plumbing: `src/llm/client.rs` vs `src/mcp/http.rs`

| Aspect | `llm/client.rs` (559 LOC) | `mcp/http.rs` (359 LOC) | Verdict |
|--------|---------------------------|--------------------------|---------|
| SSE parsing | `resp.bytes_stream().eventsource()` + manual loop, 50 ms-poll `tokio::time::timeout`, per-event state machine in `consume_event()` (content/reasoning/tool_calls_map/`in_reasoning`) (~lines 270–440) | Same `bytes_stream().eventsource()` in `read_sse_response()` (`:186–225`), simpler loop, no sub-stream state machine | Same transport pattern, different event payloads. **Shared skeleton duplicated.** |
| Timeouts/watchdogs | Three-tier watchdog constants (`INITIAL_RESPONSE_WATCHDOG_SECS=300`, `INTER_CHUNK_WATCHDOG_SECS=60`, `OVERALL_READ_TIMEOUT_SECS=1200`), repeated `elapsed() >= timeout` checks in 4 separate loop sites (send-fut, first event, consume loop, overall) | Single flat `request_timeout = 30 s` per `stream.next()` | Divergent policy; both hand-roll the same "poll stream with a clock" idiom. |
| Retry logic | `chat_stream` loop (`:159–181`): attempt counter, `MAX_ATTEMPTS=3`, `BACKOFF_BASE_MS × attempt` linear backoff, `ChatError::is_retryable()` matching 503/429/502/504 + timeout variants | **None** — a failed MCP POST fails immediately (no retry/backoff anywhere in file) | Asymmetric: the retry state machine exists only once but is generic enough (retry on {503,429,502,504} + transport error) that MCP calls should share it. |
| Error mapping | Typed `ChatError` enum (`HttpStatus{status,body}`, `InitialTimeout`, `StallTimeout`, `ReadTimeout`, `Stream`, `Transport`) + `is_retryable()` | Ad-hoc `anyhow!` strings with interpolated server/method names, `JsonRpcResponse::into_result` maps JSON-RPC errors to `anyhow` | Two error philosophies for the same failure classes (HTTP status, SSE stall, timeout). |
| reqwest client config | `default_http_client()`: `connect_timeout(10s)`, `unwrap_or_default()` | `Client::builder().timeout(30s)` built inline in `connect()` | Near-identical builder calls, duplicated. |
| POST boilerplate | `builder.post(url).json(body)` + conditional bearer_auth, then `.send()` with a hand-rolled cancellation/timeout loop | `post()` and `send_notification()` (`:119`, `:227`) each re-do header setup (`Content-Type`, `Accept`, `Mcp-Session-Id` capture) — **these two functions also duplicate each other's ~10 lines of request construction** | Internal duplication *within* mcp/http.rs on top of cross-file duplication. |

**~Duplicated LOC:** ~120–150 (SSE pump skeleton, timeout idiom, retry loop, client builder; plus ~20 inside `mcp/http.rs`).
**Risk: HIGH for retries (silent behavioral gap), MEDIUM for SSE/error plumbing.** The LLM path has battle-tested watchdogs/retries while the MCP path has neither — a flaky remote MCP server surfaces as hard failures with no backoff.
**Consolidation:** Extract a `crate::net` (or `crate::transport`) module:
- `AsyncPump`/`stream_with_watchdog(stream, initial_deadline, inter_chunk)` async iterator implementing the 50 ms poll + watchdog once;
- `retry_with_backoff(attempts, base_ms, op)` generic helper returning `Result<T, E: Retryable>` where both `ChatError` and a new `McpError` impl the same marker trait;
- shared `build_http_client(connect_timeout, total_timeout)`.
Then `llm/client.rs` keeps only its SSE payload state machine (`consume_event`) and `mcp/http.rs` keeps only JSON-RPC envelope logic.


## 4. Test fixture / mock harness duplication

**a) `test_manager()` factory — 5 near-identical copies (same body, drifted signatures).**

| Location | Signature drift | Plan path drift |
|----------|-----------------|-----------------|
| `src/orchestrator/tests.rs:5` | `&TempDir` | `Plan::at(dir.path())` |
| `src/manager/loop_tests.rs:260` (+ alias `temp_manager`) | `&Path` | `Plan::at(dir)` |
| `src/ui/session_tests.rs:203` | `&TempDir` | `Plan::at(dir.path())` |
| `tests/test_dynamic_prompts.rs:19` | `&TempDir` | **`Plan::tmp.path().join(".marmel")` — different plan root** |
| `tests/test_orchestrator.rs:46` | `&TempDir` | `Plan::at(dir.path())` |

Each body is the same 6 lines (`ChatClient::new("http://localhost:9999/v1", "test-model")`, `Plan::at(...)`, `Arc::new(HarnessStats::new())`). **~30 LOC duplicated** across crate-boundary (lib tests can't share `tests/common/`), forcing copy-paste on every side.

**b) SSE payload builder — 2 byte-identical builders in different crates.**
`src/llm/client_tests.rs:6 fn sse_body(text)` and `tests/common/mod.rs fn completion_sse(text)` produce the exact same `data: {json}\n\ndata: [DONE]\n\n` OpenAI chunk. A third variant appears inline in `tests/test_specialist_stream.rs` (3 hand-written `"data: …"` literals). Any change to chunk shape must be made 2–3×.

**c) Mock `Renderer` impls — 5 implementations of a 10-method trait, 4 of which are test doubles.**
Real: `src/ui/tui/mod.rs`, `src/ui/raw.rs`. Doubles: `src/ui/session_tests.rs RecordingRenderer`, `src/ui/bridge.rs TestRenderer`, `tests/test_ui_session.rs ScriptedRenderer`. The three doubles each re-implement the full trait surface (init/on_event/flush/rehydrate_ui/rehydrate_messages/set_subagents/poll/read/abort/shutdown ≈ 25–35 lines each, ~90 LOC total), with **no shared base double** and with divergent `poll_input`/`read_input` queue semantics (see §2 — this is why the divergence went unnoticed).

**~Duplicated LOC:** ~120–150 (factories 30 + SSE builders ~30 + doubles ~90, counting only the boilerplate portion).
**Risk: MEDIUM** — semantic drift between doubles and production renderers (queue semantics, abort flags) silently weakens test coverage; fixture drift (`Plan::at` vs `.marmel` subdir) means one integration test exercises a different on-disk layout than the lib tests.
**Consolidation:**
1. Move `test_manager` into a small public test-support module in the crate (e.g. `#[cfg(any(test, feature = "test-support"))] pub mod testing`) with a `&Path` signature; delete all 5 local copies + alias.
2. Expose `sse_body`/`completion_sse` from that same support module so both lib tests and `tests/` share it; rewrite the inline literals in `test_specialist_stream.rs`.
3. Provide a single `RecordingRenderer` base (recording events, optional input queue, abort flag) exported under test-support; have the three local doubles become 5–10-line subclasses/wrappers around it.


## 5. Config parsing/validation repetition

`src/config.rs` (578 LOC) is otherwise well-centralized (single `Config::load`, single `PartialConfig` merge, env-var fallbacks for `MARMEL_AUTH_TOKEN` / `MARMEL_BACKEND_URL` / `MARMEL_MODEL`). The real duplication is **per-module re-derivation of environment/test state**:

**a) `is_test_runner` detection — 3 identical copies (~9 lines each).**
The exact same block (current_exe contains `/deps/` or `\deps\` AND `MARMEL_LIVE_TEST` unset) appears at:
- `src/agents/runner/mod.rs:42–48` (`run_specialist` fallback decision),
- `src/agents/runner/mod.rs:65–72` (`try_run_specialist_live` gate) — duplicated *within the same file*,
- `src/agents/prompt_builder.rs:214–220` (offline-blueprint gate).

**Risk: MEDIUM-HIGH** — this predicate decides whether specialist execution is live vs. canned; it's fragile (string-matches on `target/.../deps/` paths, breaks for any other build layout) and a fix must be made in 3 places or behavior diverges between prompt synthesis and execution.

**b) `.marmel` plan-path construction — 5 ad-hoc sites.**
`.join(crate::manager::phase::MARMEL_DIR)` + filename join is re-derived independently at `src/agents/validation.rs:163,180`, `src/agents/runner/execution.rs:132`, `src/orchestrator/workers.rs:291`, `src/orchestrator/freeze.rs:260`, `src/ui/tui/render.rs:150–152`. Not a copy-paste (each joins a different filename) but there is no single `paths::plan_file()`/`paths::archive_dir()` accessor — `phase.rs` defines `MARMEL_DIR` and `PLAN_FILE` yet doesn't expose the composed path.

**~Duplicated LOC:** ≈ 35 (test-runner blocks ~27 + scattered path joins ~8).
**Risk: MEDIUM.**
**Consolidation:** Add `crate::env::is_test_runner() -> bool` (single impl, ideally based on a compile-time test flag or env var instead of exe-path sniffing); add `phase.rs::plan_path()` / `archive_dir()` accessors and point all five join sites at them.


## 6. Per-specialist agent boilerplate & fix-loop scaffolding

### 6a. `src/agents/{coder,debugger,planner,researcher,validator,generalist}.rs` — lean, NOT the hotspot

Each specialist file is only **42–81 lines** and mostly *data* (role prompt `include_str!`, tool-namespace slice, `may_recurse`). `diff coder.rs debugger.rs` shows only name/prompt/namespace-line differences; constructor/registration logic lives once in `catalog.rs` + `mod.rs`. Two minor duplications remain:
- Each file embeds a near-identical 6-line `#[cfg(test)] mod tests` (name check, 2–4 `tool_namespaces().contains(&"…")` asserts with **raw string literals** — e.g. `coder.rs:56–58`, `debugger.rs:56–58`, `planner.rs:50–52`, `researcher.rs:49–51`, `validator.rs:69–72`) instead of a shared `assert_namespaces(agent, &[…])` helper. ~35 LOC total.
- Prompt-assembly knowledge is duplicated between the per-file doc comments ("Allowed tool namespaces: …") and the actual namespace slices in code — drift risk only.

**Risk: LOW.** Consolidate the embedded tests into one parameterized test in `agents/mod.rs`.

### 6b. Agent execution loop scaffolding — **triple-duplicated**, the real hotspot

Three copies of the same "validator/specialist LLM fix-loop" exist, each ~150–290 lines:

| Loop | Location | Size |
|------|----------|------|
| `run_automated_validation_inner` | `src/agents/validation.rs:149` → ~380 | ~230 LOC |
| `run_plan_validation_inner` | `src/agents/validation.rs:572` → ~835 | ~260 LOC |
| specialist execution loop | `src/agents/runner/execution.rs:107` → end (~450) | ~340 LOC |

The two loops **inside validation.rs are nearly line-for-line copies**; verbatim identical blocks include:
- validator backend/token/model config fallback chain (`validator_backend_url.or_else(validator…).unwrap_or(cfg…)`) — 12 lines × 2,
- `ChatClient::new_with_token` + `ContextEngineFactory…specialist_context(prompt, brief)` setup,
- tool assembly: `ToolDef::default_tools()` filtered by `val_entry.allows(name)` + MCP server fan-out (14 lines × 2),
- worker registration (`register_active_worker_with_token`) + `HarnessMonitor::new_with_config` + `RepetitionDetector::new` (8 lines × 2, and a **third** copy at `execution.rs:107–160`),
- the turn loop body: cancel check → `update_active_worker_context` → `emit_status(turn)` → identical 10-line `ChatRequest{temperature:0.0,…}` construction (× 3 copies total) → `PreemptibleStreamSink::register_full` + `chat_stream_resumable(...)` call with the same 9-arg signature (`execution.rs:191–208`, `validation.rs:324–345` & `695–716`) → steer/abort handling → XML rescue via `monitor.rescue_xml` → assistant message folding + verdict parsing.
- Divergences are only cosmetic: status strings, tag names (`validator-{agent}-{tid}` vs `validator-planner`), brief text, and error-vs-Ok-on-abort semantics (plan loop returns `Ok(false,…)` while the deliverable loop returns `Err(…)` on cancellation — a **behavioral inconsistency** between copies).

Additionally each copy re-reads per-specialist config via its own `match agent { Agent::Coder => …_ROLE_PROMPT, … }` arm (`validation.rs:168–174`) duplicating the catalog's role-prompt mapping.

**~Duplicated LOC:** ~500 (290 of it is validation-internal; ~200 more is shared with execution.rs).
**Risk: HIGH** — the two in-file copies have already diverged semantically (aborted → `Ok(false)` vs `Err`); any fix-loop change (e.g. new watchdog, verdict nudge logic) must be made 2–3× and has demonstrably drifted before.
**Consolidation:** Extract one generic driver:
```rust
async fn run_validator_loop(tag, cfg, brief_or_plan, role_prompt, task_id, token) -> Result<(bool, String)>
```
parameterized by (tag, brief, prompt, abort-returns-err flag, status text); keep only the prompt/brief construction in the two public wrappers. Then `runner/execution.rs` should reuse the same driver core for its streaming turn loop, leaving it responsible only for tool *dispatch* and specialist-specific state. Also centralize the role-prompt match into `validator.rs::role_prompt_for(agent)`.

