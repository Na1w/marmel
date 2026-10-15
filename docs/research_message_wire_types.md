# Research: `Message` wire type, role spelling, and session persistence

Read-only reconnaissance for the `Message::System` → `Message::User` demotion at the
request boundary (`bugreport.md`, bug M12 family). **No files were modified by this
recon.** Line numbers are against HEAD `9fab839` (`1.0.0-refactor`, working tree clean
for `src/`).

Bounded to what the demotion needs: the enum, the role spelling path, the
persist/restore path, other pattern-match sites, and available conversion helpers.

---

## 1. The `Message` enum

`src/types/wire.rs:7-28` — **yes, it derives both `Serialize` and `Deserialize`**, and it is
an *internally tagged* enum whose tag is literally the chat `role`:

```rust
7   #[derive(Debug, Clone, Serialize, Deserialize)]
8   #[serde(tag = "role", rename_all = "lowercase")]
9   pub enum Message {
10      #[serde(rename = "system")]
11      System { content: String },
12      #[serde(rename = "user")]
13      User { content: String },
14      #[serde(rename = "assistant")]
15      Assistant {
16          #[serde(default, serialize_with = "serialize_assistant_content")]
17          content: Option<String>,
18          #[serde(default, skip_serializing_if = "Option::is_none")]
19          reasoning_content: Option<String>,
20          #[serde(default, skip_serializing_if = "Vec::is_empty")]
21          tool_calls: Vec<ToolCall>,
22      },
23      #[serde(rename = "tool")]
24      Tool {
25          tool_call_id: String,
26          content: String,
27      },
28  }
```

Variant fields (exact):

| Variant | Fields |
|---|---|
| `System` | `content: String` |
| `User` | `content: String` |
| `Assistant` | `content: Option<String>`, `reasoning_content: Option<String>`, `tool_calls: Vec<ToolCall>` |
| `Tool` | `tool_call_id: String`, `content: String` |

Important consequences for the demotion:

* `System` and `User` are **struct variants with exactly one identical field** (`content: String`) —
  a role change is a pure re-construction, nothing else to carry over.
* Derives are `Debug, Clone, Serialize, Deserialize` — **no `PartialEq`/`Eq`**. A test cannot
  `assert_eq!` two `Message`s; it must use `matches!`, `role()`/`content()`, or compare
  `serde_json::to_value(..)`.
* `serialize_assistant_content` (`src/types/wire.rs:30-41`) writes `content: None` as `""` —
  the known wire trap, which is why persistence uses a separate record type (see §3).
* Accessors live in `src/types/wire.rs:43-70`:

```rust
44      pub fn role(&self) -> &'static str {
45          match self {
46              Message::System { .. } => "system",
47              Message::User { .. } => "user",
48              Message::Assistant { .. } => "assistant",
49              Message::Tool { .. } => "tool",
50          }
51      }
```
  plus `content()` (`:53-60`) and `reasoning_content()` (`:62-70`).
* Re-exported through `src/types/mod.rs:8-10` as `types::Message` (also `ChatRequest`).
* `ChatRequest` (`src/types/wire.rs:162-183`) derives **`Serialize` only** (`Deserialize` is
  not derived; responses are decoded as `ChatChunk`). Its `messages: Vec<Message>` field is
  `src/types/wire.rs:165`.

---

## 2. Where the `role` string is produced for the LLM request

**Direct serde serialization of the enum** — the `"system"` / `"user"` / `"assistant"` /
`"tool"` strings on the wire come only from the `#[serde(tag = "role", rename_all =
"lowercase")]` attributes plus the per-variant `#[serde(rename = ...)]`. There is **no
mapping function** that builds the request payload.

The single place the JSON body is produced, `src/llm/client.rs:571`:

```rust
571        let mut builder = client.post(&url).json(&req_body);
```

`Message::role()` (`src/types/wire.rs:44`) is **diagnostic/report-only** (restore refusal text
`src/session/restore.rs:437,451,1446-1447,1487-1488,1508-1509`, test assertions), never part of
the request bytes.

### The request boundary (the choke point for the demotion)

`ChatClient::try_chat_once` (`src/llm/client.rs:520`) already rewrites a **clone** of the
request before sending — this is exactly where the demotion belongs:

```rust
535        let mut req_body = req.clone();
536        req_body.stream = Some(true);
537        if req_body.model.is_empty() {
538            req_body.model = self.model.clone();
539        }
540        for msg in &mut req_body.messages {
541            if let crate::types::Message::Assistant {
542                content,
543                tool_calls,
544                ..
545            } = msg
546            {
547                if content.is_none() {
548                    *content = Some(String::new());
549                }
550                for tc in tool_calls {
551                    tc.sanitize_arguments();
552                }
553            }
554        }
```

Downstream of the rewrite: `count_tokens(&req_body.messages)` (`:561`),
`debug_log::log_llm_request(&url, &model, &req_body)` (`:568`, generic over `Serialize`,
`src/debug_log.rs:72` — it will log the *demoted* body, which is desirable), and the POST
(`:571`).

**Every outbound chat request funnels through this one function.** Entry chain:
`ChatClient::chat` → `chat_stream` (`src/llm/client.rs:~404`) →
`crate::net::retry_with_backoff(.., ChatAttempt { client, req })` → `ChatAttempt::attempt`
(`src/llm/client.rs:~513`) → `try_chat_once`. Call sites that build `ChatRequest`s:

* manager loop / resumable stream: `src/llm/stream.rs:448` (`build_request`, `messages`
  verbatim from the engine), plus continuation rebuilds `:475` / `:499`, all sent via
  `client.chat_stream(&current_req, ..)` (`src/llm/stream.rs:565`, `:580`).
* fix loop: `src/agents/runner/fix_loop.rs:285-286` (`messages: engine.messages().to_vec()`).
* steer: `src/orchestrator/steer.rs:195` and `:770` → `chat_stream` (`:220`, `:792`).
* notice: `src/orchestrator/notice.rs:1213` → `chat_stream` (`:1235`).
* prompt builder: `src/agents/prompt_builder.rs:332` (builds `Message::System` at index 0 +
  `Message::User`) → `chat_stream` (`:353`).
* recovery copies preserve `messages` untouched (`src/llm/thinking.rs:300-305`,
  `apply_recovery_report` clones params only).

So an index-guarded demotion inside the `:535-554` block covers every call site, plus
restored histories, with one edit.

---

## 3. Persistence and restore — can a restored history hold `Message::System` at index > 0? **Yes.**

### Persist (capture)

`ContextEngine::capture_manager_section` (`src/manager/context.rs:1105-1122`):

```rust
1106        let messages: Vec<MessageRecord> =
1107            self.messages.iter().map(MessageRecord::from_wire).collect();
1108        let pinned_prefix_end = pinned_prefix_end(&self.messages);
```

`MessageRecord` (`src/session/format/messages.rs:18-38`) mirrors the same tag layout
(`#[serde(tag = "role", rename_all = "lowercase", deny_unknown_fields)]`) but keeps
`content: Option<String>` faithful (`null` vs `""`); module header
`src/session/format/messages.rs:1-9` explains why the wire type is not reused. Conversion is
field-by-field, role-preserving: `MessageRecord::from_wire` (`:69-101`) and
`into_wire` (`:108-131`):

```rust
109         MessageRecord::System { content } => Message::System { content },
110         MessageRecord::User { content } => Message::User { content },
```

Records are written to `manager.json` / `workers/<key>.json` via
`src/session/commit.rs` (paths at `:77-102`; `ManagerSection.messages` is
`Vec<MessageRecord>`, `src/session/format/envelope.rs:212-214`).

### Restore

`session::restore::load_bundle` verifies the two pins and refuses otherwise
(`src/session/restore.rs:428-455`): `messages[0]` **must** be `System` (step 2,
`PinViolation`) and `messages[1]` **must** be `User` (step 3). **Nothing anywhere validates
or normalizes roles at index > 0.** The normalize pass set
(`src/session/restore.rs:519-535`, passes N0–N6) is explicitly role-preserving — N0 is
`stored.iter().cloned().map(MessageRecord::into_wire)` (`:549-553`), and it *refuses* if
decode→re-encode is not the identity (`decode_drift`, `:1400-1410`). Install is wholesale:

```rust
// src/manager/context.rs:1151-1160
1151    pub fn install_restored_messages(&mut self, messages: Vec<MessageRecord>) {
1152        assert!(messages.len() >= 2, /* pinned pair required */);
1159        self.messages = messages.into_iter().map(MessageRecord::into_wire).collect();
1160    }
```

### Evidence that mid-history `System` records really occur

* The engine documents it: `src/manager/context.rs:268-274` —
  > “The checkpoint is matched on its **content marker**, not its role: `perform_rebirth`
  > emits it as a `User` message (bug M12) while transcripts restored from a session bundle
  > (installed wholesale by `ContextEngine::install_restored_messages`) **can still carry the
  > historic `System` variant**. … the restored bytes **MUST NOT be re-role'd or re-ordered**
  > (A §4 R-PIN-3).”
* Live test fixture: `src/session/manager_capture.rs:143-146` appends
  `Message::System { content: format!("{REBIRTH_CHECKPOINT_PREFIX}checkpoint written by an older build)") }`
  — i.e. a `System` at **index 2** — and `src/session/manager_capture.rs:353` asserts
  `matches!(restored.messages()[2], Message::System { .. })` after a capture→restore round trip.
* `pinned_prefix_end` (`src/manager/context.rs:275-293`) and its record-side twin
  `pinned_prefix_end_records` (`src/session/restore.rs:769-783`) both match
  `System { content } | User { content }` on content, i.e. the codebase already tolerates both
  spellings of a mid-history checkpoint.
* Current `perform_rebirth` (`src/manager/context.rs:1051-1064`) already emits the checkpoint as
  `Message::User`, so *new* in-memory collapses are clean — the remaining exposure is (a)
  older bundles, (b) any future mid-history injection.

**Design conclusion:** the demotion must be a *wire-boundary* transform on the cloned
`req_body.messages`, never a rewrite of the in-memory history or the durable records — restore
byte-identity invariants (R-PIN-3, `src/session/restore.rs:1480-1509` pin checks) forbid
re-role'ing persisted bytes, and the tests above assert the historic `System` survives restore.

---

## 4. Does any module pattern-match exhaustively on `Message::System` such that a role change breaks compilation elsewhere?

**No compile break.** The enum is unchanged (no variant added/removed), and the demotion runs
on the request clone inside `try_chat_once`, so every other consumer still sees whatever role
the engine holds. Exhaustive `match`es over wire `Message` in production code (all still
valid, none affected):

| Site | Shape | Effect if a mid-history `System` goes out as `User` |
|---|---|---|
| `src/types/wire.rs:44-51` `role()` | exhaustive | none (diagnostics only) |
| `src/types/wire.rs:53-60` `content()` | exhaustive | none |
| `src/manager/context.rs:151-160` `message_tokens` | `System \| User => count_text_tokens(content)` | identical token math for both roles |
| `src/manager/context.rs:507-519` `pairing_shape` | exhaustive | none — operates on the engine vector, not the wire clone |
| `src/manager/context.rs:275-293` `pinned_prefix_end` | `System \| User` content match | already handles both spellings; rebirth detection unaffected (acceptance criterion 3) |
| `src/manager/context.rs:322-327` `pin_witness_of` | `Some(Message::System …)` at `[0]`, `_` fallback | untouched — index 0 is never demoted |
| `src/manager/context.rs:1033-1037` `perform_rebirth` first-message read | `Some(Message::System …)` with `_` fallback | untouched — index 0 |
| `src/agents/prompt_builder.rs:335`, `src/orchestrator/steer.rs:198,773`, `src/orchestrator/notice.rs:1216` | constructors putting `System` at index 0 | untouched by the `index > 0` guard |
| `src/session/format/*` + `src/session/restore.rs` | match on `MessageRecord`, not `Message` | untouched (persistence untouched) |

Sites that would *semantically* care, and why they are safe:

* `REBIRTH_CHECKPOINT_PREFIX` detection is content-based and role-agnostic in *both* spellings
  (`src/manager/context.rs:283`, `src/session/restore.rs:777`); the prefix is also registered
  in `INJECTED_ADVISORY_PREFIXES` (`src/manager/context.rs:371`). Content markers are declared
  **load-bearing** (R-PIN-4, `src/manager/context.rs:363-366`) ⇒ **keep the `(SYSTEM: …)` text
  verbatim** when demoting; dropping it would silently move the pinned window and make an
  injected notice look like a genuine instruction.
* Request-body assertions in tests look for a `role == "system"` message at index 0
  (`tests/test_role_gating.rs:1986-1998`, `tests/test_agent.rs:85-86`,
  `src/llm/client_tests.rs:22-35`) — index 0 keeps its role, so they still pass. Do **not**
  merge adjacent `User` messages after demotion: pin byte-identity and
  `pinned_non_tool_turns` checks (`src/session/restore.rs:1524-1540`) and the
  `messages[1]` goal pin assume turn count/order.

---

## 5. Is there an existing role-conversion helper?

**No.** Nothing converts a `Message` from one role to another:

* `impl Message` (`src/types/wire.rs:43-70`) has only `role()`, `content()`,
  `reasoning_content()`.
* `MessageRecord::from_wire` / `into_wire` (`src/session/format/messages.rs:71-131`) are
  role-**preserving** mirrors.
* `with_role` in `tests/test_session_restore_workers.rs:278` is an unrelated worker-journal
  test builder (`role: Agent`), not a `Message` helper.
* A grep for `to_user|into_user|as_user|demote|with_role|to_system|into_system|reroled` over
  `src/` and `tests/` finds nothing relevant.

⇒ Express the demotion **inline** at the request boundary (adding a shared helper is optional;
if added, `src/types/wire.rs` `impl Message` is the natural home).

---

## Recommended expression of the demotion

Replace the header of the existing rewrite loop (`src/llm/client.rs:540`) with an
index-enumerated loop, and demote with a **struct-variant re-construction** —
`Message::User { content }`, moving the `String` out with `std::mem::take` so no clone and no
borrow conflict (the `&mut String` borrow ends before `*msg` is assigned):

```rust
for (index, msg) in req_body.messages.iter_mut().enumerate() {
    // Strict providers allow `role:"system"` only at index 0 (bug M12).
    if index > 0 {
        if let crate::types::Message::System { content } = msg {
            *msg = crate::types::Message::User {
                content: std::mem::take(content),
            };
        }
    }
    if let crate::types::Message::Assistant { content, tool_calls, .. } = msg {
        if content.is_none() {
            *content = Some(String::new());
        }
        for tc in tool_calls {
            tc.sanitize_arguments();
        }
    }
}
```

Key syntax facts to rely on:

* `Message::User { content }` — struct variant, single `String` field; `content` can be moved
  out of the matched `System` arm.
* Because `msg: &mut Message`, use `std::mem::take(content)` (or `content.clone()`, harmless —
  `req_body` is already a clone) and then assign `*msg = …`; a plain
  `let Message::System { content } = msg else { .. }; *msg = …` will **not** borrow-check.
* Guard on `index > 0` (equivalently `.iter_mut().skip(1)`), keeping `messages[0]` as `system`.
* Leave `content` byte-identical (keep the `(SYSTEM: REBIRTH CHECKPOINT…` prefix), do not merge
  adjacent user turns, and do not apply the demotion to the engine vector, to
  `MessageRecord`s, or to `capture_manager_section` — that would violate R-PIN-3 and break
  `src/session/manager_capture.rs:353`.

Test sketch (matching bugreport acceptance criterion 4; `Message` has no `PartialEq`):

```rust
// src/llm/client_tests.rs — capture the wire body via the existing wiremock pattern
// (`wiremock::matchers::path("/chat/completions")`, e.g. `:68`, `:131`)
// input:  [system, user, system]  ->  roles on the wire: ["system", "user", "user"]
let roles: Vec<&str> = body["messages"]
    .as_array().unwrap().iter()
    .map(|m| m["role"].as_str().unwrap())
    .collect();
assert_eq!(roles, ["system", "user", "user"]);
```

Unit-test surface already available: `src/llm/client_tests.rs` (wiremock body capture),
`src/manager/context_tests.rs` (rebirth + `REBIRTH_CHECKPOINT_PREFIX` detection, e.g. `:973-990`,
`:1684-1820` for role-vs-content checks), `tests/test_llm.rs:97-101` (wire serde role/tag
round-trip).
