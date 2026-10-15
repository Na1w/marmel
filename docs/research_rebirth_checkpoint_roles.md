# Recon: rebirth checkpoint roles vs. request-boundary role demotion

Read-only reconnaissance. No source file was modified (only this document was added).
Verified against `HEAD = 9fab839` (`1.0.0-refactor`), working tree clean for tracked files
(`git status --porcelain` shows only untracked `bugreport.md`, `marmel.toml.df`, `.marmel.old*`).

Question answered at the bottom: **Q5 — does a clone-only demotion keep rebirth detection
working?** → **YES** (§6).

---

## 0. State of HEAD vs. the bug report (important, the report is stale)

`bugreport.md:1-8` describes `perform_rebirth` injecting the checkpoint as
`Message::System` at index 3. **That is no longer what HEAD does.** The `User`-spelling
fix ("bug M12") is already committed:

`src/manager/context.rs:1062-1065`
```rust
        collapsed.push(Message::User {
            content: checkpoint,
        });
```

`git log -S "collapsed.push(Message::User {" -- src/manager/context.rs` → introduced in
`974b601 Refactor part 1` (already in `HEAD`).

Likewise `bugreport.md:49` ("The `REBIRTH_CHECKPOINT_PREFIX` checks in `src/manager/context.rs`
match on `Message::System` and would have to follow the role change") is **stale**: every
checkpoint detector at HEAD matches `System | User` **by content** — see §2.

Mid-history `Message::System` can nevertheless still reach the wire, which is why the
requested normalization is still meaningful: **restored session bundles** install the
historic `System` spelling verbatim (`src/manager/context.rs:269-274`,
`src/session/format/messages.rs:108` `MessageRecord::System → Message::System`, and the
"must NOT re-role/re-order" rule A §4 R-PIN-3). Live fixtures proving the shape exists:
`src/session/manager_capture.rs:144-146` and `tests/test_session_restore_manager.rs:1014,1031-1033`
(`MessageRecord::System { content: checkpoint }`).

---

## 1. `ContextEngine::perform_rebirth` — collapse code and docstring

`src/manager/context.rs:1001-1078`. Docstring (verbatim, `:1001-1029`):

```rust
    /// Collapse the history into the rebirth checkpoint shape (REQ-CORE-004).
    ///
    /// - `messages[0]`: static system prompt (REQ-CORE-001).
    /// - `messages[1]`: original user goal (REQ-CORE-002).
    /// - `messages[2]`: the last **genuine** user instruction, when there is
    ///   one (see below).
    /// - last: the `SYSTEM: REBIRTH CHECKPOINT` injection, emitted as a
    ///   **`User`** message (bug M12: strict providers reject a non-leading
    ///   `system` turn). The [`REBIRTH_CHECKPOINT_PREFIX`] text is unchanged,
    ///   and [`pinned_prefix_end`] keeps pinning it through later compactions.
    ///
    /// Two history-hygiene rules (bugs M6/M12, `docs/recon_bugs_manager.md`):
    ///
    /// 1. Runtime-injected advisories/notices ([`INJECTED_ADVISORY_PREFIXES`]:
    ///    the `CONTEXT BUDGET ADVISORY`, the legacy `CONTEXT LIMIT EXCEEDED`
    ///    notice, the post-rebirth continuation injection, and an earlier
    ///    checkpoint) are never summarized as `messages[2]` — the real last
    ///    instruction would be lost behind boilerplate.
    /// 2. The `messages[2]` slot is **omitted** when the only candidate is the
    ///    pinned goal itself, so the goal appears exactly once in the collapsed
    ///    transcript instead of being duplicated verbatim at `[1]` and `[2]`.
    ///    Rebirth therefore collapses to 4 messages when a distinct instruction
    ///    exists and to 3 when it does not.
    ///
    /// The collapse also preserves the assistant `tool_calls` ↔ `Tool` pairing
    /// invariant: every emitted message is `System`/`User`, so no `tool_calls`
    /// entry and no orphan `Tool` can survive a rebirth.
    ///
    /// The `session_rebirths` counter in the attached [`HarnessStats`] is
    /// incremented.
```

Body (`:1030-1066`) — the collapse itself:

```rust
    pub fn perform_rebirth(&mut self, summary: &str) {
        let initial_tokens = self.token_count();
        let initial_msgs = self.messages.len();
        let system = match self.messages.first() {                 // :1033-1036
            Some(Message::System { content }) => content.clone(),
            _ => String::new(),
        };
        let goal = match self.messages.get(1) {                    // :1037-1040
            Some(Message::User { content }) => content.clone(),
            _ => String::new(),
        };

        // Determine the last user instruction distinct from the goal, skipping
        // every runtime-injected advisory/notice message (bug M6).
        let last_user_instruction = self.messages.iter().skip(2).rev().find_map(|m| match m {
            Message::User { content } if *content != goal && !is_injected_advisory(content) => {
                Some(content.clone())                              // :1043-1049
            }
            _ => None,
        });

        let checkpoint = format!("{REBIRTH_CHECKPOINT_PREFIX}{summary})");   // :1051
        let mut collapsed = vec![
            Message::System { content: system },                   // :1053
            Message::User { content: goal },                       // :1054
        ];
        // M12: never duplicate the pinned goal at `[2]`.
        if let Some(instruction) = last_user_instruction {         // :1056-1060
            collapsed.push(Message::User { content: instruction });
        }
        collapsed.push(Message::User {                             // :1062-1064  <-- checkpoint
            content: checkpoint,
        });
        self.messages = collapsed;                                 // :1065 (in-memory write)
```

Consequences for the fix under review:

* The checkpoint is **not** role-anchored anywhere; only `[0]`/`[1]` are role-anchored
  (`:1033-1040`, and `pin_witness_of` `:322-333` — see §2).
* "index 3" is only true when a distinct instruction exists; otherwise the vector is
  3 messages and the checkpoint sits at `[2]` (`:1055-1061`, doc `:1022-1027`).

The only caller path: `src/harness/mod.rs:378-399` (`handle_rebirth`, gated by the
`consecutive_rebirths()` **counter**, not by a history scan), reached via
`dispatch_with_engine` (`src/harness/mod.rs:401-408`) / `dispatch_for_with_engine`.

---

## 2. Every use of `REBIRTH_CHECKPOINT_PREFIX` (and related markers)

Marker definitions:

| Constant | Location | Value (leading bytes) |
|---|---|---|
| `REBIRTH_CHECKPOINT_PREFIX` | `src/manager/context.rs:95` | `(SYSTEM: REBIRTH CHECKPOINT. The previous turn-by-turn history has been compacted. Summarized progress: ` |
| `REBIRTH_ADVISORY_MESSAGE` | `src/manager/context.rs:105` | `(SYSTEM: CONTEXT BUDGET ADVISORY…` |
| `INJECTED_ADVISORY_PREFIXES` | `src/manager/context.rs:367-373` | 5 entries incl. `REBIRTH_CHECKPOINT_PREFIX` (`:371`) |
| `SESSION_RESTORE_NOTICE_PREFIX` | `src/session/format/mod.rs:70` | `(SYSTEM: SESSION RESTORED` |
| post-rebirth continuation notice (inline literals) | `src/agents/runner/execution.rs:34-37`, `src/ui/session.rs:3020-3023`, `src/agents/validation.rs:403,550` | `(SYSTEM: Rebirth checkpoint accepted…` |

**`src/markers.rs` contains no rebirth/compaction marker at all** — `grep -n "REBIRTH\|rebirth"
src/markers.rs` returns nothing. The rebirth grammar lives solely in
`src/manager/context.rs` (+ `src/session/format/mod.rs:70`).

### Production uses (non-test)

| # | File / function | Line | Snippet | Role-sensitive? |
|---|---|---|---|---|
| P1 | `manager::context::pinned_prefix_end` | `275-287` | `Message::System { content } \| Message::User { content } => content.starts_with(REBIRTH_CHECKPOINT_PREFIX)` | **No** for `System`/`User` (explicitly role-agnostic, doc `:269-274`). Only `Assistant`/`Tool` are excluded, so a `System→User` demotion is invisible to it. |
| P2 | `manager::context::is_injected_advisory` | `377-381` | `INJECTED_ADVISORY_PREFIXES.iter().any(|p| content.starts_with(p))` | **No** — signature takes `&str`; role cannot matter. |
| P3 | `manager::context::perform_rebirth` (slot-2 scan) | `1043-1049` | `Message::User { content } if … && !is_injected_advisory(content)` | Content-only test, but the **arm** only inspects `User` messages: a legacy `System` checkpoint is never even considered as an instruction. Demotion to `User` keeps it excluded because `REBIRTH_CHECKPOINT_PREFIX` ∈ `INJECTED_ADVISORY_PREFIXES` (`:371`). |
| P4 | `manager::context::compact` | `853`, re-check `909`, `931` | `let prefix_end = pinned_prefix_end(&self.messages)…` | inherits P1 → role-agnostic. |
| P5 | `ContextEngine::capture_manager_section` | `1108` | `pinned_prefix_end(&self.messages)` (witness written to disk) | inherits P1. |
| P6 | `ContextEngine::pinned_prefix_end` (public seam) | `1182-1184` | `pinned_prefix_end(&self.messages)` | inherits P1. |
| P7 | `session::restore::pinned_prefix_end_records` | `769-783` | `MessageRecord::System { content } \| MessageRecord::User { content } => content.starts_with(REBIRTH_CHECKPOINT_PREFIX)` | **No** — doc `:766-768` says explicitly "The two `System` and `User` spellings … are matched on content, exactly like the original". |
| P8 | restore cross-checks | `restore.rs:254-255`, `479-487` (step 4), `658-666` (N3) | compare `section.pinned_prefix_end` with `pinned_prefix_end_records(...)` | inherits P7 → role-agnostic between System/User. |
| P9 | `manager::context::pin_witness_of` | `322-333` | `Some(Message::System { content }) => content.len()` for `[0]`, `Some(Message::User { … })` for `[1]` | **Yes**, but only for index **0/1**. Any demotion that leaves index 0 alone is invisible. Same for `perform_rebirth` `[0]`/`[1]` reads (`:1033-1040`) and restore refusal R-PIN-5 steps 2/3. |
| P10 | `manager::context::message_tokens` | `151-156` | `Message::System { content } \| Message::User { content } => count_text_tokens(content)` (plus constant `3`) | **No** — token accounting is identical for the two variants, so budget/trigger arithmetic is unchanged by a demotion. |
| P11 | `manager::context::pairing_shape` | `507-519` | `Message::System { .. } => ("system", …)` | Role-sensitive, but used **only** to decide whether `ensure_tool_call_pairs` changed anything (`:992-997`) — it runs on in-memory `self.messages`, never on a wire clone. |
| P12 | `session::format::messages` | `77`, `108` | `Message::System ↔ MessageRecord::System` | Role-preserving verbatim mapping (capture/restore), untouched if the in-memory vector is untouched. |

### Test uses (must keep passing / pin the grammar)

| File:line | What it pins | Role dependence |
|---|---|---|
| `src/manager/context_tests.rs:283-289` | `msgs[3]` is `Message::User` carrying the prefix | asserts the **new** `User` shape |
| `:605-611` | checkpoint survives compaction (`User \| System` match) | role-agnostic |
| `:974` + `:985-990` | fixture injects a **`System`-spelled** checkpoint inside the pinned window; asserts it stays pinned | proves content-matching covers the legacy `System` spelling |
| `:1305` | fuzz fixture: mid-transcript `Message::System` checkpoints | legacy spelling |
| `:1541-1543` | pinned checkpoint survives the `TargetUnreachable` path | content-only |
| `:1677-1733` | M6: no injected advisory text lands in `[2]` | content-only |
| `:1736-1775` | M12: `last.role() == "user"`, prefix preserved, still pinned after a later `compact()` | asserts `User`, pinning by content |
| `:1780-1817` | goal stated exactly once; 3-message collapse | role of `[0]` asserted as `System` |
| `tests/test_context.rs:352-383` | `msgs[0].role()=="system"`, `msgs[1].role()=="user"`, `last.role()=="user"` + prefix, `len()==4` | mixed: pins `[0]`/`[1]` roles, checkpoint role = user |
| `src/harness/mod.rs:1168-1235` | collapse shape: `[0] System`, `[1] User`, `[2] User` w/ prefix, goal once | asserts `User` checkpoint |
| `src/session/manager_capture.rs:130-146, 587-627, 633-680` | legacy `System` checkpoint round-trips byte-identically; `restored.pinned_prefix_end() == 3` "found by content match" | legacy spelling must keep working |
| `src/session/manager_capture.rs:109,593,620,677` | `REBIRTH_CHECKPOINT_PREFIX` stays registered in `INJECTED_ADVISORY_PREFIXES` | text-only |
| `tests/test_session_restore_manager.rs:1006-1075` | a `MessageRecord::System` checkpoint is opaque, unpruned, still `is_injected_advisory` | legacy spelling |
| `tests/test_session_restore_dangling.rs:43,175` | checkpoint text used in a restore fixture | content-only |
| `src/session/restore.rs:1876` | test helper `checkpoint()` builds `MessageRecord::User` | current spelling |

---

## 3. Other rebirth/compaction detection in the history

* `src/manager/loop.rs` — **no** rebirth/compaction detection. `grep` for
  `compact|rebirth|checkpoint|Message::` yields only two doc-comment mentions
  (`:12`, `:86`). The 544-line module is turn budget / steer queue / tool routing.
* `src/ui/session.rs` — detection is via engine **counters/ratios**, never by scanning
  roles: `ctx.should_compact()` (`:2144`, `:2305`… region), `ctx.should_advise_rebirth()`
  (`:2151`, `:2309`, `:3488`), `ctx.compact()` + `surface_compaction_outcome`
  (`:2146`, `:3907`), `ctx.reset_consecutive_rebirths()` (`:2302`, `:2364`, `:3065`),
  `ctx.rebirth_advisory_emitted` / `consecutive_rebirths` persisted at `:1203-1204`.
  Post-rebirth continuation notices are appended as **`Message::User`** text
  (`:3020-3023`).
* `src/agents/runner/execution.rs:1108-1110`, `src/agents/runner/fix_loop.rs:1203-1205` —
  same ratio-based advisory; `fix_loop.rs:724-741` (`append_tool_result`) special-cases a
  successful `rebirth` **by tool name** (`tool_names::TOOL_REBIRTH`) and injects the
  notice as a `User` turn.
* `src/session/restore.rs` — the only *other* content-based checkpoint detector
  (`pinned_prefix_end_records`, §2 P7) plus its cross-checks (§2 P8).
* `src/orchestrator/` — no checkpoint/marker detection (`grep REBIRTH_CHECKPOINT_PREFIX`
  hits nothing there); `orchestrator::tests::test_active_worker_context_tokens_rebirth_reduction`
  exercises `perform_rebirth` only.
* `src/ui/transcript.rs:44` `UiRecord::Status { .. }` ("System, rebirth, or status
  message") is a UI journal variant, decoupled from `ContextEngine::messages()`.
* `src/llm/*` — no rebirth awareness at all today (`grep` for
  `demote|REBIRTH|checkpoint` in `src/llm/*.rs` returns nothing);
  `apply_recovery` (`src/llm/thinking.rs:300-305`) and the continuation builders
  (`src/llm/stream.rs:470-514`) operate on `ChatRequest` copies and inject
  `Message::User` notices.

---

## 4. What the named tests assert about roles (the must-not-break set)

**`tests/test_role_gating.rs`** — role-gating here means **tool dispatch** roles
(`ToolCaller::{Manager,Specialist}`), not chat-message roles. Message-role assertions:

* `all_specialists_and_validator_permitted_rebirth` (`:203-251`): after
  `dispatch_for_with_engine(rebirth, …)`,
  `assert_eq!(engine.messages().len(), 4, "Rebirth must collapse {role} context to
  exactly 4 messages")` (`:245-248`). Fixture builds `[system, user(goal), user("Do work"),
  assistant]` (`:213-224`) → 4-message collapse. It checks **count only**, no role — but it
  depends on the collapse still happening (i.e. `perform_rebirth` still reads `[0]`/`[1]`
  and the injected-advisory grammar still excludes boilerplate).

Current status: **24 passed, 1 failed** — `test_prompt_based_tool_gating`
(`tests/test_role_gating.rs:312`, "Validator with run_command in prompt must be permitted
to execute run_command"). That failure is **pre-existing at HEAD** and unrelated to
message roles/rebirth (it is about prompt-based tool allowlists).

**`tests/test_context.rs`** (`test_integration_rebirth_checkpoint_shape_after_injected_advisory`,
`:345-385`): `msgs.len()==4`; `msgs[0].role()=="system"`; `msgs[1].role()=="user"` and
content == goal; `msgs[2].content()` == the genuine instruction, `!=` advisory text;
`last.role()=="user"` **and** `starts_with(REBIRTH_CHECKPOINT_PREFIX)`; goal appears
exactly once; `assert_valid_openai_pairing(msgs)`. Also `:195-196` and `:311` assert
`matches!(msgs[0], Message::System{..})` / `msgs[1], Message::User{..}` after compaction,
and `:244-245` asserts an exact role sequence `["system","user","assistant","tool","tool"]`
after pairing repair. Status: **8 passed, 0 failed**.

**`src/manager/context_tests.rs`** — the authoritative role contract (all listed in §2):
`[0]` must stay `Message::System`, `[1]` must stay `Message::User`, the checkpoint must be
`Message::User` with byte-identical prefix (`:1736-1775`), and **legacy `System`
checkpoints must keep being pinned by content** (`:974`, `:1305`).
Status (lib tests, filter `checkpoint rebirth`): **14 passed, 0 failed**.

⇒ The invariant set a request-boundary demotion must not touch: `messages[0]` role,
`messages[1]` role, checkpoint *content*, collapse length, and the byte-identity of
restored transcripts.

---

## 5. Where a clone-only demotion would live (verified plumbing)

`src/llm/client.rs:520-556` is the single choke point through which **every** chat request
passes (`ChatAttempt::attempt` → `try_chat_once`, `:403`):

```
535:        let mut req_body = req.clone();          // deep copy, owned by this fn
536:        req_body.stream = Some(true);
540:        for msg in &mut req_body.messages {      // existing rewrite loop
541:            if let crate::types::Message::Assistant {
542:                content,
543:                tool_calls,
544:                ..
545:            } = msg
546:            {
547:                if content.is_none() { *content = Some(String::new()); }
550:                for tc in tool_calls { tc.sanitize_arguments(); }
553:            }
554:        }
562:        let prompt_tokens = crate::manager::context::count_tokens(&req_body.messages);
568:        crate::debug_log::log_llm_request(&url, &req_body.model, &req_body);
571:        let mut builder = client.post(&url).json(&req_body);
```

Callers hand over **copies**, never engine storage:

* `src/agents/runner/fix_loop.rs:286-288` — `messages: engine.messages().to_vec()`
  (used by `fix_loop.rs:893` and `execution.rs:429`).
* `src/ui/session.rs:2176` — `let msgs = ctx.messages().to_vec();`.
* `src/orchestrator/steer.rs:195-201`, `:770-776`, `src/orchestrator/notice.rs:1213-1219`,
  `src/agents/prompt_builder.rs:332-339` — literal `[System, User]` requests (system at
  index 0 only, so a demotion that spares index 0 is a no-op for them).
* `src/llm/stream.rs:448-468` `build_request(messages: Vec<Message>)`, plus
  `build_continuation_request` / `build_fallback_continuation_request` (`:470-514`) which
  rebuild `req.messages` from `base_messages.to_vec()`.

Nothing reads `req_body`/`req.messages` back into the engine: after a turn the engine is
extended with the **response** only (`src/ui/session.rs:2274` region: `ctx.append(assistant)`;
fix-loop appends assistant + `Tool` results via `append_tool_result`,
`fix_loop.rs:724-745`).

Wire encoding: `src/types/wire.rs:8-28` — `#[serde(tag = "role", rename_all = "lowercase")]`,
so the only way to change the emitted role is to change the **variant**
(`Message::System` → `Message::User`); `Message::role()` (`:44-51`) is display/test-only.

---

## 6. Q5 — clone-only demotion: does rebirth detection keep working?

**YES.** Detection is entirely a function of the **in-memory `self.messages` content**, and
every consumer of that content already matches the checkpoint by prefix rather than by role:

1. **Pinning** — `pinned_prefix_end` (`src/manager/context.rs:275-287`) matches
   `Message::System { content } | Message::User { content }` on
   `content.starts_with(REBIRTH_CHECKPOINT_PREFIX)`. A clone-side demotion leaves
   `self.messages` untouched; and even an in-memory demotion would still pin, because the
   arm already accepts `User`. Its doc (`:269-274`) states the intent verbatim: "matched on
   its **content marker**, not its role".
2. **Advisory grammar** — `is_injected_advisory` (`:377-381`) takes `&str`; role cannot
   enter. `perform_rebirth`'s slot-2 scan (`:1043-1049`) excludes the checkpoint through
   `INJECTED_ADVISORY_PREFIXES` (`:367-373`, entry `:371`) — text-only.
3. **Pins `[0]`/`[1]` are untouched** — `perform_rebirth` `:1033-1040` and
   `pin_witness_of` `:322-333` are the only role-sensitive history readers, and both look
   exclusively at indices 0 and 1. `bugreport.md` acceptance criterion 2 ("the system
   message at index 0 keeps its role") is therefore also a *correctness* requirement, not
   just a UX one: gate the demotion on `index > 0`.
4. **Budget arithmetic is role-blind** — `message_tokens` (`:151-156`) charges `System` and
   `User` identically, so `should_compact()` / `should_advise_rebirth()` / the
   `count_tokens(&req_body.messages)` accounting at `client.rs:562` cannot shift.
5. **Capture/restore is unaffected** — `capture_manager_section` (`:1105-1123`) snapshots
   `self.messages` verbatim (`MessageRecord::from_wire`, `session/format/messages.rs:77`)
   plus the witnesses `pinned_prefix_end` (`:1108`) and `pin_witness_of`. Since the engine
   vector is never rewritten, persisted bytes and witnesses stay identical, and the
   restore-side recomputation `pinned_prefix_end_records` (`session/restore.rs:769-783`)
   already accepts both spellings, so its cross-checks (`:479-487`, `:658-666`) cannot
   start refusing bundles.
6. **`pairing_shape` (`:507-519`) is never exposed to the clone** — it only observes
   `self.messages` inside `ensure_tool_call_pairs` (`:992-997`), so a clone-side role flip
   cannot make `ensure_pairing_at_boundary` (`src/ui/session.rs:2169-2171`) report a
   phantom repair.
7. **Test evidence** — the legacy `System`-checkpoint fixtures already pass alongside the
   `User` code path: `context_tests.rs:974/985-990`, `:1305`,
   `session/manager_capture.rs:144-146` + `:620-627` (`restored.pinned_prefix_end() == 3`,
   "the pinned window is found by content match"),
   `tests/test_session_restore_manager.rs:1006-1075` (a `MessageRecord::System` checkpoint
   survives and is still `is_injected_advisory`). Measured at HEAD: `cargo test --lib --
   checkpoint rebirth` → 14 passed / 0 failed; `cargo test --test test_context` → 8 passed /
   0 failed; `cargo test --test test_role_gating` → 24 passed / 1 failed (pre-existing
   `test_prompt_based_tool_gating`, `tests/test_role_gating.rs:312`, unrelated to roles).

Corroborating prior recon: `docs/research_request_boundary.md:201-206` and `:295-305`
reach the same conclusion independently ("In-memory rebirth detection is **role-agnostic**
already … Demoting only the cloned wire body leaves this untouched").

Residual risks to keep in mind (all outside detection):
adjacent `User` messages after demotion are already a normal shape today
(`stream.rs:504-512`), and the `(SYSTEM: …)` **text** prefix must not be stripped when the
variant is demoted — stripping it would break `pinned_prefix_end`,
`is_injected_advisory`, and the byte-identity restore contract (`context.rs:362-366`,
R-PIN-4).
