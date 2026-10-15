# Recon: the LLM request boundary (`ChatClient::try_chat_once`)

Read-only reconnaissance for the requested fix in `bugreport.md`: demote
`Message::System` at index > 0 to `Message::User` on the **outgoing** request only,
so the wire body keeps `system` at index 0. All line numbers are from the current
HEAD (`9fab839`, branch `1.0.0-refactor`), no files were modified.

---

## 1. `ChatClient::try_chat_once` — signature and control flow

**Only direct caller** (private fn, same `impl` block): `src/llm/client.rs:399-404`

```rust
399|     async fn attempt(
400|         &mut self,
401|         progress: &mut StreamProgress<F>,
402|     ) -> Result<StreamedReply, ChatError> {
403|         self.client.try_chat_once(self.req, progress).await
404|     }
```

`ChatAttempt<'a>` (`client.rs:387-390`) is the `crate::net::RetryOp` adapter; it is
driven by `ChatClient::chat_stream` (`client.rs:486-518`) through
`crate::net::retry_with_backoff(MAX_ATTEMPTS, BACKOFF_BASE_MS, …)` (`client.rs:496-503`).
`ChatClient::chat` (`client.rs:468-470`) is `chat_stream(req, |_| true)`.
**Consequence:** the body clone and every rewrite below happen *once per attempt*,
inside `try_chat_once` — a normalization placed there covers retries too.

### Signature (`src/llm/client.rs:520-527`)

```rust
520|     async fn try_chat_once<F>(
521|         &self,
522|         req: &ChatRequest,
523|         progress: &mut StreamProgress<F>,
524|     ) -> Result<StreamedReply, ChatError>
525|     where
526|         F: FnMut(&str) -> bool,
527|     {
```

Body spans `client.rs:528-806` (fn closes at `:806`; `impl ChatClient` closes at `:807`).

### Control flow, in order

| Lines | Step |
|---|---|
| 528 | `let client = &self.client;` (the `reqwest::Client` field, struct at `:233-240`) |
| 530-533 | URL built: `format!("{}/chat/completions", self.backend_url.trim_end_matches('/'))` |
| **535** | **`let mut req_body = req.clone();`** — the one and only deep clone of the outgoing `Vec<Message>` (`ChatRequest: Clone`, `types/wire.rs:162` — actually `#[derive(Debug, Clone, Serialize)]`) |
| 536 | `req_body.stream = Some(true);` |
| 537-539 | model fallback: `if req_body.model.is_empty() { req_body.model = self.model.clone(); }` |
| **540-554** | **the existing per-message rewrite loop — the insertion neighbourhood** |
| 556-561 | `tracing::info!` with `req_body.messages.len()` |
| 562-563 | `let prompt_tokens = crate::manager::context::count_tokens(&req_body.messages); record_tokens_in(prompt_tokens);` — token accounting reads the **already-rewritten** body |
| 564-567 | `tracing::debug!("LLM request body: {}", serde_json::to_string(&req_body)…)` |
| 568 | `crate::debug_log::log_llm_request(&url, &req_body.model, &req_body);` — persisted request dump |
| **571-574** | **the single wire send**: `let mut builder = client.post(&url).json(&req_body);` + optional `bearer_auth` |
| 576-618 | first-event watchdog loop via `crate::net::pump_future` (send deadline = `initial_timeout_secs`); errors → `ChatError::Transport` / `InitialTimeout`; abort → `cancelled_before_stream(...)` |
| 620-632 | non-2xx → `ChatError::HttpStatus { status, body }` |
| 634 | `resp.bytes_stream().eventsource()` |
| 637-695 | `ReadState::default()`, default terminal `StreamEnd`; first event consumed; early-exit finalize paths (`:673-683`, `:688-694`) |
| 697-773 | main SSE consume loop (inter-chunk watchdog, `consume_event`, overall `OVERALL_READ_TIMEOUT_SECS` wrapper at `:774`) |
| 779-794 | terminal classification + single `state.into_accumulator().finalize(&url, &req_body.model, req_start, terminal, progress)` |
| 796-805 | `Ok(reply)` / mid-stream retry forbid `Err(progress.forbid_mid_stream_retry(cause, reply))` |

### Where message content is rewritten before sending (all of it)

```rust
540|         for msg in &mut req_body.messages {
541|             if let crate::types::Message::Assistant {
542|                 content,
543|                 tool_calls,
544|                 ..
545|             } = msg
546|             {
547|                 if content.is_none() {
548|                     *content = Some(String::new());
549|                 }
550|                 for tc in tool_calls {
551|                     tc.sanitize_arguments();
552|                 }
553|             }
554|         }
```

* Empty-assistant-content fill: `client.rs:547-549`.
* `ToolCall::sanitize_arguments()` on every tool call: `client.rs:550-552`
  (definition `src/types/wire.rs:136-139` → `ensure_valid_json_arguments` `:88-117`).
* **No role rewriting happens anywhere on this path today.** This is the only
  pre-send mutation site in the codebase.

Note: `ChatError` is `pub(crate)` (`:247-248`) and `is_retryable`
(`:281-291`) retries only `503 | 429 | 502 | 504` — so the strict-backend
`400 "System message must be at the beginning."` is a **single attempt**, surfaced
verbatim through `chat_stream`'s `Err` arm (`:510-516`).

---

## 2. Exact outgoing message type and how `role` gets onto the wire

Outgoing messages are a **strongly typed own enum**, never `serde_json::Value`:

* `ChatRequest.messages: Vec<Message>` — `src/types/wire.rs:163-165`;
  `ChatRequest` is `#[derive(Debug, Clone, Serialize)]` (`:161-162`), i.e. serialize-only.
* The enum — `src/types/wire.rs:7-28`:

```rust
 7| #[derive(Debug, Clone, Serialize, Deserialize)]
 8| #[serde(tag = "role", rename_all = "lowercase")]
 9| pub enum Message {
10|     #[serde(rename = "system")]
11|     System { content: String },
12|     #[serde(rename = "user")]
13|     User { content: String },
14|     #[serde(rename = "assistant")]
15|     Assistant {
16|         #[serde(default, serialize_with = "serialize_assistant_content")]
17|         content: Option<String>,
...
23|     #[serde(rename = "tool")]
24|     Tool { tool_call_id: String, content: String },
28| }
```

`role` is therefore an **internally-tagged serde tag** produced by `#[serde(tag = "role")]`
plus the per-variant `rename` (`:10`, `:12`, `:14`, `:23`). It is never a free-form
string on this path — the only way to change the emitted role is to change the
variant. (`Message::role()` at `:44-51` returns `&'static str` and is used only for
display/token-shaping, not serialization.)

* Assistant `content: None` is force-serialized as `""` by
  `serialize_assistant_content` (`wire.rs:30-41`) — which is why the explicit
  `content.is_none()` fill at `client.rs:547-549` is belt-and-braces for the same
  "backend rejects null" class of 400.
* Re-export: `Message` comes from `crate::types` (`src/types/mod.rs:7-9`);
  `client.rs` already refers to it as `crate::types::Message` (`:541`).

---

## 3. Call sites that reach `try_chat_once`

Single funnel: everything goes through `ChatClient::chat` (`client.rs:468`) or
`ChatClient::chat_stream` (`client.rs:486`).

| File | Line | Function | Route |
|---|---|---|---|
| `src/llm/client.rs` | 403 | `ChatAttempt::attempt` (`:399`) | direct, via `chat_stream` retry loop |
| `src/llm/stream.rs` | 565, 580 | `chat_stream_resumable` (`:529`) | `client.chat_stream` (main + fallback continuation) |
| `src/llm/stream.rs` | 705 | `chat_client_turn` (`:675`) | `chat_stream_resumable` |
| `src/orchestrator/steer.rs` | 220 | `arbitrate_steer_context_stream` (`:134`) | `client.chat_stream` |
| `src/orchestrator/steer.rs` | 792 | `synthesize_steer_subtask_response` (`:736`) | `client.chat_stream` |
| `src/orchestrator/notice.rs` | 1235 | `evaluate_worker_reply` (`:1197`) | `client.chat_stream` |
| `src/agents/prompt_builder.rs` | 353 | `AgentBlueprintBuilder::synthesize_with_llm` (`:310`) | `client.chat_stream` |
| `src/agents/runner/fix_loop.rs` | 904 | `run_fix_loop` (`:828`) | `llm::chat_stream_resumable` |
| `src/agents/runner/fix_loop.rs` | 1257 | `stream_single_turn` (`:1238`) — called from `src/agents/runner/execution.rs:441` | `llm::chat_stream_resumable` |
| `src/ui/session.rs` | 2201 | `run_session_with_bounds_report` (`:1673`) — the Manager turn loop (`msgs = ctx.messages().to_vec()` at `:2176`) | `chat_client_turn` |
| `src/agents/validation.rs` | 273, 463 | validator entry points (`ChatClient::new_with_token`, then `run_fix_loop` at `:408`, `:555`) | via fix loop |
| `src/harness/mod.rs` | 825 | `reply_to_arbitrator` tool path (`ChatClient::from_config` → `evaluate_worker_reply` at `:828`) | via notice |

Manager loop (`src/manager/loop.rs`) never touches the LLM directly — it only bounds
the session (`src/manager/loop.rs:32` comment points at `ui/session.rs`).

---

## 4. Any bypass of `try_chat_once` for chat/completions?

**No.** Exhaustive check:

* `grep '\.post('` over `src/` (excluding tests) returns exactly two hits:
  `src/llm/client.rs:571` (`client.post(&url).json(&req_body)`) and
  `src/mcp/http.rs:452` (`.post(&self.endpoint)` — MCP JSON-RPC transport, not chat).
* `src/net/http.rs` contains only `build_http_client` (`:31-41`) and
  `build_http_client_with` (`:44-53`) — reqwest client construction, no requests.
  `src/net/mod.rs` re-exports only `build_http_client`, retry (`MAX_ATTEMPTS`,
  `BACKOFF_BASE_MS`, `RetryOp`, `Retryable`, `retry_with_backoff`) and sse pumps
  (`PumpNext`, `SSE_POLL_INTERVAL`, `pump_future`, `pump_next`).
* `src/llm/stream.rs` never builds a request itself for the wire: it composes
  `ChatRequest`s (`build_request` `:448-468`, `build_continuation_request` `:470-495`,
  `build_fallback_continuation_request` `:497-514`) and always hands them to
  `client.chat_stream(...)`.
* `drive_streamed_turn` (`src/llm/stream.rs:356`) takes an injected `chat` closure and
  can reach no `ChatClient` at all — its only caller is `src/llm/stream_tests.rs:13`
  (still re-exported at `src/llm/mod.rs:14`). Test-only, no production bypass.
* `apply_recovery` / `NudgePolicy::nudge` (`src/llm/thinking.rs:496-501`,
  `src/llm/stream.rs:811-815`, `:836-838`) inject notices as **`Message::User`**
  (`"SYSTEM NOTICE: …"` text), not `Message::System`.

Mid-history `Message::System` sources that the normalization must therefore catch:

* `src/manager/context.rs:1030-1065` — `perform_rebirth` today already emits the
  checkpoint as `Message::User` (`:1062-1064`), but the code documents that restored
  bundles still carry the historic `System` spelling: `src/manager/context.rs:269-274`
  and the fixture `src/session/manager_capture.rs:144-146`
  (`ctx.append(Message::System { … REBIRTH_CHECKPOINT_PREFIX … })`).
* Restore installs the vector verbatim — `src/session/format/messages.rs:108`
  (`MessageRecord::System → Message::System`) and the "MUST NOT re-role/re-order"
  rule (`context.rs:273-274`, A §4 R-PIN-3). So the request boundary is the correct
  (and only safe) place.
* In-memory rebirth detection is **role-agnostic** already:
  `pinned_prefix_end` (`src/manager/context.rs:275-290`) matches
  `Message::System { content } | Message::User { content }` on
  `REBIRTH_CHECKPOINT_PREFIX` (constant at `:95`). Demoting only the cloned wire body
  leaves this untouched.

---

## 5. Existing test conventions

### `src/llm/client_tests.rs` (840 lines, included via `#[cfg(test)] #[path = "client_tests.rs"] mod tests;` at `src/llm/client.rs:1088-1090`)

Mixed style, both available:

* **Wiremock mock backend** (dominant): `wiremock::MockServer` +
  `Mock::given(method("POST")).and(path("/chat/completions"))` and an SSE body built
  by helpers `sse_body` (`:9-20`), `content_chunk` (`:398-406`), `tool_call_chunk`
  (`:409-426`), `DONE` (`:428`), shared mount helper `sse_mock` (`:431-448`).
* **Client construction**: `ChatClient::new(server.uri(), "test-model".to_string())`
  (`:85`, `:142`, `:458`, `:792`) — no builder/config needed; `with_client`
  (`client.rs:445`) exists but is unused in these tests.
* **History construction**: plain `ChatRequest` literal helper `request()`
  (`:22-36`) with a single `Message::User`; tests mutate fields on the clone
  (`:779-783`) before `client.chat(&…)`.
* **Asserting on the body actually sent** — exact precedent for the new test,
  `test_llm_recovery_request_body_stays_in_documented_ranges` (`:752-840`):
  ```rust
  762|     let bodies: Arc<Mutex<Vec<serde_json::Value>>> = Arc::new(Mutex::new(Vec::new()));
  768|             move |req: &Request| {
  769|                 let body: serde_json::Value =
  770|                     serde_json::from_slice(&req.body).unwrap_or(serde_json::Value::Null);
  771|                 bodies.lock().unwrap().push(body);
  772|                 ResponseTemplate::new(200).set_body_string(sse_body("recovered"))
  ```
  then `sent["temperature"] == json!(MAX_TEMPERATURE)` etc. (`:800-820`).
* **Pure-function unit tests** (no server): `test_llm_uses_shared_policy_values`
  (`:41-51`), `test_chat_error_retryability_matches_shared_policy` (`:192-235`),
  `test_consume_event_does_not_double_count_tokens` (`:311`),
  `test_consume_event_marks_done_abort_and_continue` (`:352`),
  `test_chat_client_default_and_custom_watchdogs` (`:384-395` — reaches private fields,
  possible because the module is `#[path]`-included into `client`).
* Note `build_request`/`StreamConfig` are `pub(crate)` and reachable from these tests
  (`:708-712`).

### `tests/test_llm.rs` (310 lines, integration crate `marmennill`)

* Pure-function serialization tests, no HTTP at all:
  `test_integration_chat_request_payload_construction` (`:34-59`) builds a
  `ChatRequest` with `Message::System` + `Message::User` and asserts on
  `serde_json::to_string(&req)`; `test_assistant_message_content_never_serializes_as_null`
  (`:61-102`) asserts on the serialized message and on `serde_json::from_str` of
  `{"role":"assistant","content":null}`.
* Real-socket fake backend: `mod raw_backend` (`:108-…`) with
  `raw_backend::frame` (`:116`), `Backend::spawn` and `base_url()`; used by
  `test_stream_cut_after_deltas_sends_exactly_one_request` (`:239-…`) and
  `test_failure_before_any_delta_is_still_retried_once` (`:279-…`) via
  `ChatClient::new(backend.base_url(), "test-model")` (`:246`, `:294`) +
  `client.chat(&raw_backend::request())`.
* A third body-matcher precedent exists in `src/ui/bridge/arbiter.rs:832-845` using
  wiremock's `body_string_contains(...)` matcher.

**Recommendation:** put the regression test in `src/llm/client_tests.rs` next to
`:752-840`, reusing `sse_body`/`request()` and the `bodies: Arc<Mutex<Vec<Value>>>`
capture — assert `sent["messages"]` roles for `[system, user, system]` →
`["system","user","user"]`, plus the index-0-keeps-role guard. `Message` has no
`PartialEq`, so compare the serialized roles (or `body["messages"][i]["role"]`).

---

## Exact insertion point

**Function:** `ChatClient::try_chat_once`, `src/llm/client.rs`.

Fold the demotion into the **existing single normalization pass at
`src/llm/client.rs:540-554`** (immediately after the per-attempt clone at `:535`),
turning the loop header into an enumerated one:

```rust
540|         for (idx, msg) in req_body.messages.iter_mut().enumerate() {
541|             if idx > 0
542|                 && let crate::types::Message::System { content } = msg
543|             {
544|                 *msg = crate::types::Message::User {
545|                     content: std::mem::take(content),
546|                 };
547|             }
548|             if let crate::types::Message::Assistant { content, tool_calls, .. } = msg { … }  // unchanged, ex-541-553
549|         }
```

Why exactly there:

1. It is after the only body clone (`:535`) and before every consumer of the body:
   `count_tokens` (`:562`), the debug body dump (`:564-567`),
   `debug_log::log_llm_request` (`:568`) and the single `.json(&req_body)` send
   (`:571`) — so logs, token accounting and the wire body all agree.
2. It runs once per retry attempt (the clone/normalization live inside the retried
   call), and covers every call site listed in §3 with one edit.
3. It touches only `req_body`, never the caller's `Vec<Message>` (`ui/session.rs:2176`
   passes a `to_vec()` clone; `chat_stream_resumable` clones again at `stream.rs:550-551`),
   so `REBIRTH_CHECKPOINT_PREFIX` detection (`context.rs:275-290`), the pin witness
   (`context.rs:322-330`) and session restore byte-identity (R-PIN-3) are unaffected.
4. Ordering inside the loop is irrelevant to correctness: a `System` variant never
   matches the `Assistant` arm at `:541`, and a demoted `User` still gets the same
   token count (`context.rs:151-153` treats `System` and `User` identically).
5. `[lints]` in `Cargo.toml` deny `unused`/`dead_code` and `clippy::all`, so keep it a
   plain in-place `match`/`let … = ` guard (edition 2024 letschains `&&` onto `let`;
   already used at e.g. `src/manager/context.rs` / `ui/session.rs:1813`).

Optional companion (out of scope for this recon): the open questions in
`bugreport.md` (keep the `(SYSTEM: …)` text? merge adjacent user messages?) are both
invisible to the code above — content is preserved verbatim and adjacent `User`
messages are already produced by `build_fallback_continuation_request`
(`stream.rs:504-512`) today.
