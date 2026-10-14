# Recon: duplication in the network / HTTP / streaming layer

Crate `marmennill`, workspace `/home/fredrik/marmel/wip` (read-only recon, no source modified).
Scope inspected in full: `src/net/{mod,http,retry,sse}.rs`, `src/llm/{mod,client,stream}.rs`,
`src/mcp/{mod,client,http}.rs`, plus their test modules (`src/llm/client_tests.rs`,
`src/llm/stream_tests.rs`, `src/mcp/http_tests.rs`) and integration guards in `tests/`.

## 0. Grounding verification (claims from the task)

| Claim | Verdict | Evidence |
|---|---|---|
| `src/llm/stream.rs` (878 L) is StreamSink/demux/drive, **no** SSE chunk parser | **CONFIRMED** | `eventsource` appears only at `src/llm/client.rs:6,312,477` and `src/mcp/http.rs:11,192`; `stream.rs` consumes already-decoded `&str` deltas (`TurnStreamHandler::on_chunk_with_sink`, `src/llm/stream.rs:326`) and contains no `ChatChunk`/`event.data` decoding. Sole chunk parser = `consume_event`, `src/llm/client.rs:476-556`. |
| `src/net/sse.rs` is a shared pump consumed by the LLM client | **CONFIRMED for LLM, NOT shared overall** | call sites `src/llm/client.rs:276` (`pump_future`), `:321`, `:378` (`pump_next`). The MCP transport does **not** use it (`src/mcp/http.rs:192-224` hand-rolls `tokio::time::timeout(self.request_timeout, stream.next())`). |
| SSE/event handling is duplicated between `src/llm/client.rs` and `src/mcp/http.rs` | **PARTIAL** | framing is delegated to the `eventsource_stream` crate on both sides; only the *poll loop wrapper* is duplicated (cluster C4), payload decoding is protocol-specific and legitimately separate (C5). |
| Retry policy is "shared by both clients" (`src/net/retry.rs:2-10`, `src/llm/client.rs:16-18`) | **FALSE (doc drift)** | `retry_with_backoff` (`src/net/retry.rs:40-70`) has **zero call sites** in `src/` and `tests/`; `McpError` referenced at `src/net/retry.rs:23` does not exist anywhere; MCP HTTP has no retries at all. |

## 1. Verdict summary

| # | Cluster | Verdict | Locations |
|---|---|---|---|
| C1 | Retry/backoff loop: inline copy vs unused shared helper | **REAL** | `src/llm/client.rs:192-210` vs `src/net/retry.rs:40-70` |
| C2 | Retry/`Retryable` policy absent from MCP transport; doc contract drift | **REAL (gap)** | `src/net/retry.rs:2-10,23` vs `src/mcp/http.rs:119-185,227-268` |
| C3 | reqwest client construction | **REAL** | `src/mcp/http.rs:100-104` vs shared `src/net/http.rs:12-24` (used only at `src/llm/client.rs:79-81`) |
| C4 | "Poll a stream/future against a deadline" mechanics | **REAL** | `src/mcp/http.rs:192-224`, `src/mcp/client.rs:139-166` vs `src/net/sse.rs:36-84` |
| C5 | SSE event → JSON payload handling | **PARTIAL** | `src/llm/client.rs:312,476-556` vs `src/mcp/http.rs:192,203-208` |
| C6 | HTTP non-2xx → read body → build error; retryable-status table | **PARTIAL** | `src/llm/client.rs:298-311`, `:99-110`; `src/mcp/http.rs:162-174`, `:259-267`; `src/mcp/client.rs:150-157` |
| C7 | MCP JSON-RPC call sites: `initialize` / `list_tools` / `call_tool` / POST plumbing | **REAL** | `src/mcp/client.rs:200-277` vs `src/mcp/http.rs:270-354`; `src/mcp/http.rs:126-157` vs `:233-258` |
| C8 | MCP result parsing duplicated again inside its own test file | **REAL** | `src/mcp/http.rs:294-315`/`328-350` vs `src/mcp/http_tests.rs:102-118`/`148-176` |
| C9 | `TurnStreamHandler` twin methods (orchestrator-sink vs generic-sink) | **REAL** (one twin dead) | `src/llm/stream.rs:274-317` vs `:326-385`; `:387-397` vs `:399-405` |
| C10 | Streamed-turn drive loop skeleton duplicated (nudge/recovery/transcript) | **PARTIAL** | `src/llm/stream.rs:413-485` vs `:692-878` (incl. `rescue_xml` block `:459-465` vs `:759-765`, empty-production tail `:469-479` vs `:860-868`) |
| C11 | `StreamedReply` finalize block repeated 3×, `consume_event` call repeated 2× | **REAL** (intra-file) | `src/llm/client.rs:340-353`, `:355-367`, `:435-461`; `:328-335` vs `:381-388` |
| C12 | MCP debug-log instrumentation asymmetry | **PARTIAL** | `src/mcp/client.rs:124,151-157,174-181` vs none in `src/mcp/http.rs` |
| — | JSON-RPC envelope types + `id_matches` / `into_result` | **FALSE (already shared)** | `src/mcp/http.rs:20-80`, imported by `src/mcp/client.rs:14` |
| — | `ChatRequest` construction / continuation prefill builders | **FALSE** | single site `src/llm/stream.rs:496,517,544` |
| — | SSE framing (chunk→event) | **FALSE** | both sides use `eventsource_stream::Eventsource` |

## 2. Detailed findings

### C1 — Retry loop: shared helper exists but is dead code (REAL)
Shared, never called (`src/net/retry.rs:40-70`):
```rust
pub async fn retry_with_backoff<E, T, S, F, Fut>(max_attempts: u32, base_ms: u64, tag: &str,
    state: &mut S, mut op: F) -> Result<T, E>
where E: Retryable, F: FnMut(&mut S) -> Fut, Fut: Future<Output = Result<T, E>> { ... }
```
Independent copy in `src/llm/client.rs:192-210`: identical `attempt += 1` / `e.is_retryable() && attempt < crate::net::MAX_ATTEMPTS` guard, identical `BACKOFF_BASE_MS * attempt` sleep, identical warn/error log text (`"LLM backend call attempt {attempt}/{max} failed ({e}), retrying in {ms}ms..."`). Only the constants are shared. The justification comment (`src/llm/client.rs:187-191`, "the helper's boxed-Future signature cannot express" the borrowed `on_delta`) is **stale**: the helper is `FnMut(&mut S) -> Fut` with a `state: &mut S` parameter explicitly documented (`src/net/retry.rs:36-38`) for exactly this `on_delta` case, and it takes `&mut on_delta` as `S`. Verdict REAL, unification is a drop-in.

### C2 — MCP transport never retries; shared-policy claim is false (REAL gap)
`src/net/retry.rs:2-10` states the MCP client "now shares this exact policy"; `src/net/retry.rs:23` names `mcp::http::McpError` as a `Retryable` implementor. Grep across `src/`: the only `Retryable` impl is `ChatError` (`src/llm/client.rs:99-110`); `McpError` does not exist. `HttpSseConnection::post`/`send_notification` (`src/mcp/http.rs:119-185`, `:227-268`) return `anyhow::Error` and fail on first 5xx/connection error. Consequence: policy text, retryable-status knowledge (`503/429/502/504`), and backoff constants exist once but are applied once.

### C3 — reqwest builder bypass (REAL)
`src/net/http.rs:12-24` is the intended single builder, used at `src/llm/client.rs:79-81`
(`build_http_client(Some(10s), None)`). `src/mcp/http.rs:100-104` still does
`reqwest::Client::builder().timeout(Duration::from_secs(30)).build().with_context(...)` — same logic, independent copy (and it drops `connect_timeout` coverage). Verdict REAL.

### C4 — Poll-with-deadline loop re-implemented in both MCP transports (REAL)
`src/mcp/http.rs:192-224`:
```rust
let mut stream = resp.bytes_stream().eventsource();
loop {
    let ev = tokio::time::timeout(self.request_timeout, stream.next()).await.map_err(|_| anyhow!("timeout waiting for SSE response ..."))?;
    match ev { Some(Ok(event)) => {...}, Some(Err(e)) => {...}, None => {...} }
}
```
`src/net/sse.rs:36-58` (`pump_next`) implements the same shape (deadline check → bounded poll → classify `IdleTimeout`/`Item`), and `pump_future` (`:63-84`) is the one-shot variant already used for `send()` at `src/llm/client.rs:276`. `src/mcp/client.rs:139-149` re-implements it a third time over `read_line` with a hard-coded `Duration::from_secs(30)` (`:140`) that duplicates `HttpSseConnection::request_timeout` (`src/mcp/http.rs:89,111`). Semantics differ slightly (MCP: fresh flat timeout per event, no abort hook; LLM: 50 ms poll + per-call deadline policy + `on_delta("")` abort) → both are expressible through the existing net primitives (`pump_next` with `Some(per_event_deadline)`, `pump_future` for `read_line`, abort `&mut |_| true`). Verdict REAL mechanics duplication, PARTIAL policy.

### C5 — SSE event payload handling (PARTIAL)
`src/llm/client.rs:488-495` special-cases `[DONE]` then `serde_json::from_str::<ChatChunk>(&ev.data)`; `src/mcp/http.rs:203-208` does `serde_json::from_str::<JsonRpcResponse>(&event.data) && parsed.id_matches(id)` (skip-until-match). Same envelope shape, different wire contracts — a shared "SSE JSON decoder" abstraction would buy nothing except a second indirection; only the *loop* (C4) should be unified.

### C6 — HTTP status error mapping (PARTIAL)
`src/llm/client.rs:298-311`: `status.is_success()` → `resp.text().await.unwrap_or_default()` → `ChatError::HttpStatus { status, body }` + `tracing::error!` + `debug_log::log_llm_error`.
`src/mcp/http.rs:162-174` (post) and `:259-267` (notification): same read-body-then-format pattern, two different `anyhow!` message shapes; `src/mcp/client.rs:150-157` is the analogous "server closed stdout" mapping. The retryable-status table is inline in `ChatError::is_retryable` (`src/llm/client.rs:103-110`). Real duplication is small (≈8 lines × 3) but it is the piece blocking C2.

### C7 — MCP transport call sites (REAL)
`diff` output confirms only the dispatch call differs:
* `initialize`: `src/mcp/client.rs:200-216` vs `src/mcp/http.rs:270-287` — byte-identical `init_params` literal (`protocolVersion: "2024-11-05"`, `clientInfo` from `CARGO_PKG_VERSION`) at `src/mcp/client.rs:201-209` and `src/mcp/http.rs:271-279`.
* `list_tools`: `src/mcp/client.rs:218-242` vs `src/mcp/http.rs:289-316` — identical tool-array walk, identical `unwrap_or_else(|| json!({"type":"object"}))` fallback (`:229`/`:304`).
* `call_tool`: `src/mcp/client.rs:244-277` vs `src/mcp/http.rs:318-354` — identical `json!({"name","arguments"})` params and identical content-array/`isError` rendering.
* POST plumbing inside `src/mcp/http.rs`: `:126-157` vs `:233-258` — identical `serde_json::to_string`, identical header pair (`Content-Type: application/json`, `Accept: application/json, text/event-stream`), identical `Mcp-Session-Id` echo + re-capture, differing only in log wording and status handling.

### C8 — Test re-implements production parsing (REAL)
`src/mcp/http_tests.rs:102-118` and `:148-176` copy the `list_tools` / `call_tool` parsing bodies inline ("Mock the logic inside list_tools", `:83`). These tests therefore do **not** guard the production code at `src/mcp/http.rs:294-315` / `:328-350` or `src/mcp/client.rs:221-242` / `:253-277`; a production parse change passes them silently. (Note one behavioural drift already visible: the test filters `inputSchema: null` with `.filter(|v| !v.is_null())` at `:110`, production does not.)

### C9 — `TurnStreamHandler` twins (REAL; one twin is dead code)
`on_chunk` (`src/llm/stream.rs:274-317`) vs `on_chunk_with_sink` (`:326-385`): ~40 lines identical (token counting, `demux.push_delta` closure with repetition detection, `tok_est = text.len().div_ceil(4)` thinking-budget logic, cancellation/budget return predicate) except the emit target (`crate::orchestrator::emit_event` vs `sink.emit`) and the extra `sink.poll_control()` pause/abort block. `finish` (`:387-397`) vs `finish_with_sink` (`:399-405`) likewise. Grep shows **no callers** of `on_chunk`/`finish`/`TurnStreamHandler::new` (`:199-214`) anywhere in `src/` or `tests/`; production path uses `with_cancellation` + `on_chunk_with_sink` (`:589,605,620`) and `finish_with_sink` (`:669`). Tests use `for_sink`/`for_sink_with_thinking_budget` (`src/llm/stream_tests.rs:134,158,180`).

### C10 — Two streamed-turn drivers (PARTIAL)
`drive_streamed_turn` (`src/llm/stream.rs:413-485`, generic over `F: FnMut(ChatRequest) -> Fut`) and `chat_client_turn` (`:692-878`, bound to `ChatClient` + `chat_stream_resumable`) share: `build_request` + `apply_recovery` prologue (`:432-437` vs `:705-710`), demux-to-sink, `tool_calls.extend`, `rescue_xml` block (`:459-465` vs `:759-765`), and the empty-production nudge tail (`:469-479` vs `:860-868`). `chat_client_turn` additionally holds three recovery branches (thinking budget `:780-807`, output budget `:810-835`, repetition `:837-858`) which are structurally the same "push synthetic assistant + SYSTEM NOTICE user message + `recovery = true` + continue" idiom three times. `drive_streamed_turn`'s only caller is its own test (`src/llm/stream_tests.rs:12`); production callers are `src/ui/session.rs:658` (`chat_client_turn`) and `src/agents/runner/fix_loop.rs:347,551` (`chat_stream_resumable` directly).

### C11 — `StreamedReply` finalize 3× in one function (REAL)
`src/llm/client.rs:340-353`, `:355-367`, `:435-461` each perform `map_to_tool_calls` → build `StreamedReply` → `count_reply_tokens` → `record_tokens_out` → `log_llm_response(..., 200, elapsed, &reply)`. The 8-argument `consume_event(...)` call is also duplicated verbatim at `:328-335` and `:381-388`. Also stale: `#[allow(clippy::never_loop)] let resp = loop { ... }` at `:273-274` — single-iteration remnant from the pre-`net::` code.

### C12 — Instrumentation asymmetry (PARTIAL)
The stdio transport logs every call (`src/mcp/client.rs:124` request, `:151-157`/`:174-181` response via `debug_log::log_mcp_request/log_mcp_response`, `src/debug_log.rs:316,333`); the HTTP transport emits nothing for the same logical events, while the LLM client logs four times per call (`src/llm/client.rs:258-262,280-292,303-310,458-460`).

## 3. Recommended unification tasks

**T1 — Delete the inline retry loop, use the shared helper** (REAL, C1)
Files: `src/llm/client.rs` (`:182-211`), optionally `src/net/retry.rs` (docs only).
Replace with `crate::net::retry_with_backoff(crate::net::MAX_ATTEMPTS, crate::net::BACKOFF_BASE_MS, "LLM backend call", &mut on_delta, |on_delta| self.try_chat_once(req, on_delta)).await.map_err(anyhow::Error::from)`. Keep the `map_err` since `ChatError` is `pub(crate)` (`:82`). Delete the stale comment at `:187-191`.

**T2 — Give MCP a retryable error type and apply the shared policy** (C2, C6)
Files: new `src/mcp/error.rs` (or `src/mcp/protocol.rs`, see T3), `src/mcp/http.rs:119-185,227-268`, `src/net/retry.rs` (fix doc lines `2-10,23`), `src/llm/client.rs:103-110` (replace inline status list with a shared `net::retry::is_retryable_status(u16) -> bool`, `pub const RETRYABLE_STATUS: [u16; 4] = [503, 429, 502, 504]`), plus `src/mcp/http_tests.rs`.
Add `net::http::read_error_body(resp) -> (u16, String)` for the shared status-body read used by `src/llm/client.rs:298-300` and both MCP HTTP sites. Direction is mcp → net only; `net` gains no upward dependency.

**T3 — Extract one MCP protocol module, keep transports thin** (C7, C8, C12)
Files: new `src/mcp/protocol.rs`; move `src/mcp/http.rs:20-80` (JSON-RPC envelopes) into it; add `initialize_params() -> Value`, `call_tool_params(&str, &Value) -> Value`, `parse_tools(&Value, &str) -> Vec<McpTool>`, `render_call_tool_result(&Value) -> Result<String>`; add a small `trait McpTransport { async fn request(&mut self, method: &str, params: Option<Value>, id: u64) -> Result<Value>; async fn notify(&mut self, method: &str, params: Option<Value>) -> Result<()>; }` and move `initialize`/`list_tools`/`call_tool` into one generic implementation; then slim `src/mcp/client.rs:200-277` and `src/mcp/http.rs:270-354` to transport primitives (`send_request`, `post`, `send_notification`, `read_sse_response`) plus a shared `fn json_rpc_headers(session_id: Option<&str>) -> HeaderMap` used by `src/mcp/http.rs:128-135` and `:235-242`. Update `src/mcp/mod.rs:6-11` re-exports and rewrite `src/mcp/http_tests.rs:88-178` to call `parse_tools`/`render_call_tool_result` for real.

**T4 — Route both MCP poll loops through `net::sse`** (C4)
Files: `src/mcp/http.rs:186-225`, `src/mcp/client.rs:139-166`, `src/net/sse.rs` (add nothing, or an `sse_event_stream(resp) -> Eventsource<ByteStream>` helper to dedupe `src/llm/client.rs:312` / `src/mcp/http.rs:192`).
MCP HTTP: `let deadline = Instant::now() + self.request_timeout; match net::pump_next(&mut stream, Some(deadline), &mut |_| true)` → map `IdleTimeout` to the existing timeout message. MCP stdio: `net::pump_future(&mut read_fut, Some(deadline), &mut |_| true)`; hoist the hard-coded 30 s (`src/mcp/client.rs:140`) into a named constant shared with `HttpSseConnection::request_timeout` (`src/mcp/http.rs:89,111`). `net` stays dependency-free.

**T5 — Collapse `TurnStreamHandler` twins** (C9)
Files: `src/llm/stream.rs:199-214,274-317,387-397` (+ `src/llm/mod.rs:12-15` exports).
Introduce `struct OrchestratorSink` implementing `StreamSink` (`emit` → `crate::ui::Event::Message/Thinking`, `is_aborted`/global-cancel check) — dependency direction unchanged, since `stream.rs` already calls `crate::orchestrator::emit_event` (`:305-308`). Then keep only `on_chunk_with_sink`/`finish_with_sink`; either delete `on_chunk`/`finish`/`new` (unreferenced) or reduce them to one-line wrappers over `OrchestratorSink`.

**T6 — One streamed-turn driver** (C10)
Files: `src/llm/stream.rs:413-485,692-878` (+ new private helpers `fn attach_rescued_tool_calls(&mut Message)`, `fn recovery_nudge(&mut transcript, assistant_note: &str, system_notice: &str, reason: &str)`, `fn finalize_turn_transcript(...) -> Message`).
Fold the three recovery branches in `chat_client_turn` into one helper keyed by an enum, and make `drive_streamed_turn` either (a) `chat_client_turn` with an injected `chat` closure, or (b) `#[cfg(test)]`-private if it stays test-only. Do **not** move any of this into `src/net` — it is policy, not transport.

**T7 — Local de-duplication inside `try_chat_once`** (C11)
Files: `src/llm/client.rs:321-461`.
Add `fn finalize(url, model, req_start, content, reasoning, raw, tool_calls_map) -> StreamedReply` (token counting + `log_llm_response`) and a `consume(&mut state, ev, on_delta) -> Result<bool, ChatError>` closure/struct so the three finalize blocks and two `consume_event` call sites collapse; drop the `never_loop` scaffold at `:273-274`.

## 4. Risks and tests to preserve

* **`src/net/*` has zero unit tests today** (no `#[cfg(test)]` in `src/net/*.rs`, nothing in `tests/`), so all pump/retry/builder behaviour is only indirectly guarded. Any T1/T2/T4 change must add `src/net` tests (or keep the client-level ones green).
* Retry/backoff: `src/llm/client_tests.rs:36` `test_llm_retry_backoff` (503→429→200, asserts exactly 3 attempts), `:67` `test_llm_retry_exhaustion` (503 ×3, error contains "503"). T1 must keep the 3-attempt/linear-backoff counts.
* Watchdog/pump: `src/llm/client_tests.rs:85` `test_llm_prefill_delay_triggers_initial_timeout_and_retries` (1 s initial watchdog vs 1.5 s server delay) and `:121` `test_llm_on_delta_abort_during_prefill` (abort via `on_delta("")` on the 2nd 50 ms tick — pins `SSE_POLL_INTERVAL = 50ms`, `src/net/sse.rs:18`, and the abort-before-deadline ordering in `pump_next`/`pump_future`, `src/net/sse.rs:44-52,71-79`). Do not make the abort hook optional or reordered.
* Chunk parsing: `src/llm/client_tests.rs:155` `test_consume_event_does_not_double_count_tokens` pins `consume_event`'s signature (it is called directly) and the global token counters — relevant to T7 (keep the 8-arg fn or update the test deliberately). `:195` pins default watchdogs (300/300) and the builder setters.
* MCP: `src/mcp/http_tests.rs:9` request serialization, `:33` `id_matches`, `:53` `into_result` (error text must keep code/message/server name) are genuine guards of the shared envelopes; `:82`/`:131` are **fake** guards (C8) — converting them to call production code in T3 is a real coverage gain and may surface drift (e.g. the `inputSchema: null` case at `src/mcp/http_tests.rs:110` vs `src/mcp/http.rs:304`).
* Stream layer: `src/llm/stream_tests.rs:4` (demux via `drive_streamed_turn` — the only caller, see T6), `:45`, `:55` (continuation builders), `:132` pause, `:156` repetition-vs-thinking, `:177` thinking budget (exercise `for_sink*` + `on_chunk_with_sink`, T5), `:204` config defaults.
* End-to-end streaming/pause/resume: `tests/test_ui_session.rs:414` `test_ui_session_stream_pause_and_resume_on_user_question`, `:501` `test_specialist_stream_preemption_and_resumption_on_shared_model` (raw TCP SSE server → exercises pump, continuation prefill, `chat_stream_resumable` at `:580`), `:341`, `:1216`; `tests/test_specialist_stream.rs:149,215,308,390,453` guard the nudge/recovery loop targeted by T6; `tests/test_llm.rs:8` guards thinking-demux over chunks.
* Behaviour changes that must be explicit, not accidental: adding retries to MCP (T2) changes latency and duplicate-side-effect risk for `tools/call` (non-idempotent POSTs) — decide whether `tools/call` should be excluded from retry; `mcp/client.rs` stdio timeout currently resets per line (`:141`) whereas `net::pump_*` deadline semantics are absolute (T4); `build_http_client` for MCP must keep `timeout(30s)` and no `connect_timeout` (`src/mcp/http.rs:100-104`) or request-timeout behaviour shifts.
* Layering guard: all proposals keep the direction `llm → net`, `mcp → net`, `mcp → mcp::protocol`, `llm::stream → llm::client`. Nothing in `src/net` may reference `llm`, `mcp`, `ui`, or `orchestrator` — `pump_*`'s abort hook stays `&mut impl FnMut(&str) -> bool` (`src/net/sse.rs:41,68`) precisely to preserve that.
