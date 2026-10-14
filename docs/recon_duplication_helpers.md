# Recon: duplicated helper / formatting / parsing logic outside the network layer

Crate `marmennill`, workspace `/home/fredrik/marmel/wip`. **Read-only recon — no source file was modified.**
Inspected: `src/agents/runner/formatting.rs`, `src/ui/tui/formatting.rs`, `src/ui/helpers.rs`,
`src/ui/transcript.rs`, `src/ui/tui/render.rs`, `src/ui/bridge/*`, `src/ui/session*`,
`src/harness/plan.rs`, `src/manager/phase.rs`, `src/manager/loop.rs`, `src/manager/context.rs`,
`src/orchestrator/{plan_summary,delegate,workers,steer,steer_extractor,preemption,mod}.rs`,
`src/harness/monitor/{xml,repetition}.rs`, `src/agents/{mod,validation,prompt_builder}.rs`,
`src/tool_names.rs`, `src/types/{tools,wire}.rs`.

## 0. Grounding verification (claims from the task)

| Claim | Verdict | Evidence |
|---|---|---|
| `src/agents/runner/formatting.rs` (~213 L) is tool-arg formatting | **CONFIRMED** | 2 pub fns: `format_tool_args_preview` (`:8-59`), `format_tool_args_full` (`:61-174`); tests `:176-213`. |
| `src/ui/tui/formatting.rs` (~955 L) is TUI formatting | **CONFIRMED** | 20 pub fns; largest: `format_math_expr` (`:211-405`, 195 L), `wrapped_lines` (`:527-647`, 121 L). |
| `src/harness/plan.rs` vs `src/orchestrator/plan_summary.rs` duplicate the `- [ ] [t-xxx]` parse | **PARTIAL / mis-pair** | `harness/plan.rs` (115 L) contains **no** plan parsing — it delegates to `Plan::pending_tasks()` (`src/harness/plan.rs:79` → `src/manager/phase.rs:387`). The real duplicate parsers are `src/manager/phase.rs`, `src/orchestrator/plan_summary.rs`, `src/orchestrator/delegate.rs`, `src/agents/prompt_builder.rs`, `src/ui/tui/formatting.rs` (cluster C3). |
| Tool-name literals are hard-coded across `src/agents/*`, `src/harness/*`, `src/orchestrator/*`, `src/ui/*` | **FALSE (already centralized)** | Only literal tool-name comparisons in non-test code: `src/agents/validation.rs:44` (`name == "leaveVerdict"` alias) and `src/harness/pty.rs:106` (binary name). All other sites use `crate::tool_names::*` constants (schema source `src/types/tools.rs:171` etc.). |
| `src/manager/context.rs` vs other tiktoken users | **REAL** | `count_tokens` (`src/manager/context.rs:69`) is the only wrapper; 9 raw `tiktoken_rs::cl100k_base_singleton().encode_ordinary(..)` call sites live in the UI layer (C8). |
| `src/harness/monitor/xml.rs` vs `src/orchestrator/steer_extractor.rs` duplicate block extraction | **FALSE** | `xml.rs` = `<tool_call>`/`<function=>` block rescuer (`src/harness/monitor/xml.rs:80-308`); `steer_extractor.rs` = incremental JSON *string-field* reader (`:17-117`). Different inputs/outputs; only a stale doc-comment copy links them (C13). |
| `src/agents/*` must not depend on `src/ui/*` | **CURRENTLY HELD** | `grep -rn "crate::ui" src/agents` → 0 hits (reverse direction `src/ui` → `crate::agents` has 32 hits). |

## 1. Verdict summary

| # | Cluster | Verdict | Locations (file:line) |
|---|---|---|---|
| C1 | Task-id bracket/quote strip (`clean_task_id`) | **REAL — worst offender** | 31 sites: `src/ui/helpers.rs:369-374` + `src/ui/tui/formatting.rs:667-670` (2 divergent named helpers) + 29 inline clones (full list §2.1) |
| C2 | Task-id *token* regex `\(?\[?(t-[A-Za-z0-9_-]+)\]?\)?` | **REAL** | `src/agents/mod.rs:99-102`, `src/manager/loop.rs:118,135-137`, `src/manager/phase.rs:174-175` (byte-identical) |
| C3 | Plan checkbox / task-line regex family + per-task-line check-off matcher | **REAL** | `src/manager/phase.rs:64-78,487-493,660-666`; `src/orchestrator/delegate.rs:79-88`; `src/orchestrator/plan_summary.rs:6-11,38-58`; `src/agents/prompt_builder.rs:494-499`; `src/ui/tui/formatting.rs:653-655,683-686,720-722` |
| C4 | `MissionMarker` enum + `parse` + `contains_failed_marker` duplicated as two distinct types | **REAL** | `src/agents/mod.rs:90-136` vs `src/manager/phase.rs:196-248` |
| C5 | `output_is_success` vs `contains_failed_marker` sanitiser tables | **PARTIAL** | `src/manager/phase.rs:83-97` vs `src/manager/phase.rs:205-213` / `src/agents/mod.rs:110-118` |
| C6 | Tool-arg display formatters (3 near-identical switches) | **PARTIAL + latent bug** | `src/agents/runner/formatting.rs:8-59`, `:61-174`; `src/ui/helpers.rs:78-156` |
| C7 | UTF-8-safe truncate / ellipsis / clip family | **REAL (+ 1 panic risk)** | `src/orchestrator/workers.rs:395-400` ≡ `:435-440`; `src/agents/runner/formatting.rs:19-24,50-55`; `src/ui/helpers.rs:119-124,147-152`; `src/ui/helpers.rs:1021-1033`; `src/manager/context.rs:129-142`; `src/harness/mod.rs:213-231`; `src/ui/helpers.rs:157-175` |
| C8 | BPE token counting (raw singleton calls) | **REAL** | `src/manager/context.rs:61-62` (helper) vs raw calls `src/ui/helpers.rs:555,679,832,842`; `src/ui/tui/events.rs:785`; `src/ui/session.rs:279,1079,1465` |
| C9 | Token count humanising (`1.2k` / `1.0M`) | **REAL** | `src/ui/tui/render.rs:956-964` vs inline `src/orchestrator/workers.rs:290-294` |
| C10 | Thinking-tag vocabulary (`<think>`/`<thought>`/`[thinking]`) | **PARTIAL** | canonical `src/llm/thinking.rs:54-58`; forks at `src/ui/tui/formatting.rs:86-88,107-109,132-138`; `src/ui/tui/render.rs:130-136,158-164`; emitters `src/ui/tui/mod.rs:1098`, `src/ui/tui/events.rs:685`, `src/llm/client.rs:339,427,491,504,517` |
| C11 | Steer-arbitration loop skeleton + context snapshot duplicated | **REAL** | `src/ui/bridge/sink.rs:81-443` (`on_pause`) vs `src/ui/bridge/steer.rs:33-301` (`spawn_steer_arbitration`); overlap ≈ 100 L each |
| C12 | `sleep` argument alias chain (`seconds`/`duration`/`duration_seconds`) | **REAL ×4** | `src/harness/sleep.rs:6-18`, `:88-96`; `src/agents/runner/formatting.rs:39-48`, `:149-158` |
| C13 | Stale XML-rescue doc block pasted onto repetition detector | **REAL (doc drift)** | `src/harness/monitor/repetition.rs:88-103` duplicates `src/harness/monitor/xml.rs:7-25` |
| C14 | `leave_verdict` arg alias chain (`verdict`/`status`/`decision`, `comments`/`comment`/`feedback`) | **PARTIAL** | `src/agents/validation.rs:51-59` vs `src/agents/runner/formatting.rs:79-91` |
| C15 | Active-subagent status block (fallback path) | **PARTIAL** | `src/ui/helpers.rs:10-35` vs `src/orchestrator/workers.rs:330-454` |
| C16 | Streaming render line filter (`tab→4sp`, tag skip, math, empty-drop) | **REAL (intra-file)** | `src/ui/tui/render.rs:127-141` ≡ `:155-169`; similar loop again `src/ui/tui/formatting.rs:434-470` |
| — | `format_tool_call_display` reuse across session/tools/bridge | **FALSE (already shared)** | single def `src/ui/helpers.rs:78`, call sites `src/ui/session/tools.rs:136,436,452`, `src/ui/session.rs:844,1197,1213`, `src/ui/transcript.rs:235` |
| — | Line-wrap accounting | **FALSE** | one impl `src/ui/tui/formatting.rs:527-647`, reused at `:502,505,661,799` and `src/ui/tui/render.rs:40-50,75,210-218` |
| — | JSON block extraction (`xml.rs` vs `steer_extractor.rs`) | **FALSE** | see §0 |

## 2. Detail

### 2.1 C1 — the single biggest cluster: task-id cleaning
Two *differently typed* helpers exist for the same rule (strip `[ ] ( ) " '` then trim):
- `src/ui/helpers.rs:369-374` → `pub fn clean_task_id(&str) -> String`
- `src/ui/tui/formatting.rs:667-670` → `pub fn clean_task_id(&str) -> &str`

and the identical closure is re-typed inline at 29 further sites:

`src/agents/prompt_builder.rs:36`, `src/agents/runner/execution.rs:67`, `src/agents/validation.rs:172,247`,
`src/harness/workspace.rs:93`, `src/manager/loop.rs:127`, `src/manager/phase.rs:484,605`,
`src/orchestrator/delegate.rs:54,74`, `src/orchestrator/mod.rs:265,414`,
`src/orchestrator/preemption.rs:47,53,82`, `src/orchestrator/steer.rs:432,462`,
`src/orchestrator/workers.rs:161,371,484,498`, `src/ui/helpers.rs:254`,
`src/ui/session.rs:807,1160`, `src/ui/session/tools.rs:99,399`, `src/ui/tui/mod.rs:725,762,829`.

Real duplication (semantic, not textual): three call-site groups re-implement *matching* on top of it —
`src/ui/helpers.rs:376-407` (`find_subagent_mut`), `src/orchestrator/preemption.rs:43-60` (`StreamIdentity::matches`),
`src/ui/tui/formatting.rs:731-772` (`line_matches_task_id`) — all “normalize both sides, compare exact / suffix”.

### 2.2 C2 — task-id token regex triplicated
`static TASK_ID_RE = \(?\[?(t-[A-Za-z0-9_-]+)\]?\)?` with an identical `find_task_id` body:
`src/agents/mod.rs:99-108` (returns `&str`), `src/manager/phase.rs:174-187` (returns `String`, comment at `:176-179`
explicitly says it is duplicated “so this module carries no dependency on the agents module”),
`src/manager/loop.rs:118,134-137` (inline in `extract_task_id`, `:121-139`).
Also duplicated alias logic: `src/manager/loop.rs:124-132` re-does the C1 strip.

### 2.3 C3 — plan-line parsing spread over five modules
Canonical: `src/manager/phase.rs:64-70` (`TASK_LINE_RE`, `ALL_TASKS_RE`), `:74-78` (`UNCHECKED_BOX_RE`, `CHECKED_BOX_RE`),
`parse_unchecked_tasks` `:660-666`, `check_off` `:476-537` (builds `(?i)^\s*(?:[-*]|\d+\.)\s*\[\s*\]\s*\*{0,2}\[?<tid>\]?\*{0,2}\b` at `:487-491`, fallback `:494-501`).
Near-clones:
- `src/orchestrator/delegate.rs:79-88` — same regex with `\[\s*[xX]\s*\]` (checked instead of unchecked) + identical `None` fallback `:84-88`.
- `src/agents/prompt_builder.rs:494-499` — `TASK_LINE_RE` + `\s*(.*?)$` capture; i.e. `parse_tasks_from_plan` (`:492-522`) is `parse_unchecked_tasks` plus description/role capture.
- `src/orchestrator/plan_summary.rs:6-11` — third checkbox/task-id regex set; `RE_CHECKBOX_DONE = \[[xX]\]` **drops the `(x)` form** that `src/manager/phase.rs:78` supports → divergence. Its “strip list markers” closure is copy-pasted twice inside the same fn (`:38-47` and `:49-58`).
- `src/ui/tui/formatting.rs:653-655` (`visual_line_offset_of_first_pending` unchecked regex), `:683-686` (`extract_plan_line_task_id` checkbox regex), `:720-722` (bracketed-tid regex) — a 4th, UI-only notion of “what is a plan task line”.

### 2.4 C4 — two `MissionMarker` types
`src/agents/mod.rs:90-95` and `src/manager/phase.rs:196-201` declare the same three-variant enum with identical doc
comments; `contains_failed_marker` (`src/agents/mod.rs:110-118` ≡ `src/manager/phase.rs:205-213`) and
`MissionMarker::parse` (`src/agents/mod.rs:122-136` ≡ `src/manager/phase.rs:222-237`) are character-for-character
equivalent except the `task_id` return type. Both copies are live but never cross paths: the plan copy is used only
inside its own module (`src/manager/phase.rs:588,612`, reached from `check_plan_on_marker` at `:587`), the agents copy
is what the rest of the system consumes (`src/agents/mod.rs:236,244`; `src/orchestrator/mod.rs:309,339,341,463,486,497,533`;
`src/orchestrator/delegate.rs:120,132,145,161,170,178`; `src/ui/bridge/steer.rs:207`).
Marker-text knowledge also leaks into `src/manager/phase.rs:83-97` (`output_is_success`) and `src/orchestrator/delegate.rs:172`.

### 2.5 C6 — three tool-arg display switches
`format_tool_args_preview` (`src/agents/runner/formatting.rs:8-59`, caps 40/30, `…`),
`format_tool_args_full` (`:61-174`), `format_tool_call_display` (`src/ui/helpers.rs:78-156`, caps 60/57, `…`, renders `name(args)`).
Divergence that is a **bug**: the schema field is `pattern` (`src/types/tools.rs:178-181`, `src/harness/search.rs:26`), but both agents-side formatters read `query` only (`src/agents/runner/formatting.rs:27-30`, `:101-110`) → grep previews render empty; the UI formatter reads `pattern` first, then `query` (`src/ui/helpers.rs:126-129`).

### 2.6 C7 — truncation family
Textually identical pair: `src/orchestrator/workers.rs:395-400` and `:435-440` (300-byte cap, `floor_char_boundary(297)`, `"..."`).
Same shape with different caps: `src/agents/runner/formatting.rs:19-24` (40), `:50-55` (30),
`src/ui/helpers.rs:119-124` (60), `:147-152` (60).
`truncate_reason` (`src/ui/helpers.rs:1021-1033`) slices `s[..max_len]` **without** `floor_char_boundary`
(→ `panic: byte index is not a char boundary` on multi-byte reasons) and uses `"..."` instead of `…`.
Boundary-safe slicing exists 3 more times: `utf8_safe_slice` (`src/manager/context.rs:129-142`),
`chunk_utf8` (`src/ui/helpers.rs:157-175`), head/tail clip loop (`src/harness/mod.rs:213-231`).

### 2.7 C8/C9 — token counting & humanising
Wrapper exists (`src/manager/context.rs:61-62 bpe()`, `:69 count_tokens`, `:74 count_assistant_tokens`) but there is no
*plain-text* counter, so 9 UI sites re-open the tokenizer: `src/ui/helpers.rs:555-557`, `:679-681`, `:832-834`,
`:842-845`; `src/ui/tui/events.rs:785-787`; `src/ui/session.rs:279-281`, `:1079-1081`, `:1465-1467`.
`format_count` (`src/ui/tui/render.rs:956-964`) ≡ inline block `src/orchestrator/workers.rs:290-294`.

### 2.8 C11 — duplicated steer arbitration loop
`src/ui/bridge/sink.rs:92-101` ≡ `src/ui/bridge/steer.rs:71-80` (plan read + `format_plan_progress_summary` +
`loop_count == 1 ? cached : get_active_subtasks_str()` + `has_active`);
`src/ui/bridge/sink.rs:106-112` ≡ `src/ui/bridge/steer.rs:64-68` (`format_steering_history` else `"None"`);
`src/ui/bridge/sink.rs:113-118` ≡ `src/ui/bridge/steer.rs:92-97` (**identical SYSTEM NOTICE wording**);
`src/ui/bridge/sink.rs:120-130` ≡ `src/ui/bridge/steer.rs:99-108` (`SteerContext` literal, same field values);
`src/ui/bridge/sink.rs:132-180` ≡ `src/ui/bridge/steer.rs:110-159` (`preempt_conflicting_stream`,
`arbitrate_steer_context_stream`, `is_global_abort` match, `complete_all`/`complete_with_subtask_decision`,
`Cancel` loop); sleep block `src/ui/bridge/sink.rs:182-214` ≡ `src/ui/bridge/steer.rs:150-180`
(only the sink emits via `Renderer`, steer via `tx.send(SteerArbEvent::Delta)` — the delta sink is the *only* real difference).

## 3. Proposed target modules + call-site updates

Neutral, leaf-level modules declared in `src/lib.rs` (alongside `tool_names`), with **no** dependency on
`src/ui/*` and no dependency on `src/agents/*` — so `agents → ui` stays forbidden and `ui → neutral` is legal.

| New module | Contents | Consumers |
|---|---|---|
| `src/task_id.rs` | `clean_task_id(&str) -> &str`, `clean_task_id_owned`, `find_task_id_token(&str) -> Option<String>`, `task_id_eq(a,b) -> bool`, `TASK_ID_TOKEN_RE` | agents, harness, manager, orchestrator, ui |
| `src/plan_parse.rs` | `TASK_LINE_RE` / `ALL_TASK_LINE_RE` / `UNCHECKED_BOX_RE` / `CHECKED_BOX_RE`, `unchecked_boxes(&str)`, `parse_tasks(&str) -> Vec<PlanTaskItem>` (id + description + role hint), `task_line_match(line, tid)`, `checked_line_regex(tid)`, `unchecked_line_regex(tid)`, `strip_list_marker(line)` | manager (owner), orchestrator, agents, ui |
| `src/markers.rs` | single `MissionMarker` enum, `parse`, `is_complete`, `contains_failed_marker`, `output_is_success` (one sanitiser table) | agents, manager, orchestrator, ui |
| `src/text_util.rs` | `ellipsize(s, max_chars_bytes, ellipsis) -> String`, `utf8_safe_slice`, `chunk_utf8`, `clip_head_tail`, `count_text_tokens(&str)` (BPE), `format_count(usize)` | agents, harness, manager, orchestrator, ui |
| `src/tool_args.rs` | alias accessors: `arg_path`, `arg_command`, `arg_pattern` (accepts `pattern` then `query`), `arg_agent`, `arg_task_id`, `arg_seconds`, `arg_verdict_pair` | agents, harness, orchestrator, ui |
| `src/think_tags.rs` | `TAG_PAIRS`, `is_bare_tag_line(&str) -> bool`, `strip_tags(&str) -> String`, `split_segments(line, &mut bool)` | llm (owner), ui |

Call-site updates (mechanical)

1. `src/task_id.rs`
   - delete `src/ui/helpers.rs:369-374` and `src/ui/tui/formatting.rs:667-670`; re-point all 29 inline sites listed in §2.1 (one-liner: replace the `trim_matches(...).trim()` chain with `task_id::clean_task_id(x)`).
   - `src/ui/tui/formatting.rs:731-772` keeps `line_matches_task_id` but delegates id compare to `task_id::task_id_eq`.
   - `src/manager/loop.rs:121-139` → `task_id::extract_from_args(args)` (uses `find_task_id_token`).
   - `src/agents/mod.rs:99-108`, `src/manager/phase.rs:174-187` → delete, call `task_id::find_task_id_token`.
2. `src/plan_parse.rs`
   - `src/manager/phase.rs:64-78,487-493,494-501,660-666` → re-export from `plan_parse` (phase keeps `Plan` I/O only).
   - `src/orchestrator/delegate.rs:72-91` → `plan_parse::is_checked_off(&content, tid)`.
   - `src/orchestrator/plan_summary.rs:6-11,33-58` → `plan_parse::{is_done,is_pending,strip_list_marker}` (fixes the missing `(x)` variant).
   - `src/agents/prompt_builder.rs:492-522` → `plan_parse::parse_tasks` (keep role-hint type in prompt_builder via `From`).
   - `src/ui/tui/formatting.rs:650-666,681-727` → `plan_parse::{first_unchecked_line, task_id_of_line}`.
3. `src/markers.rs`
   - delete `src/manager/phase.rs:196-248` copy (enum + `contains_failed_marker` + `impl` incl. `is_complete`); add
     `pub use crate::markers::MissionMarker;` in both `src/agents/mod.rs` and `src/manager/phase.rs` so the existing
     import paths keep compiling while call sites migrate (`src/manager/phase.rs:588,612`;
     `src/agents/mod.rs:236,244`; `src/orchestrator/mod.rs:309-533`; `src/orchestrator/delegate.rs:120-178`;
     `src/ui/bridge/steer.rs:207`). This avoids a `manager → agents` edge (agents already references manager 9×).
   - `src/manager/phase.rs:83-97` → `markers::output_is_success`.
4. `src/text_util.rs`
   - replace `src/orchestrator/workers.rs:395-400` and `:435-440`, `src/agents/runner/formatting.rs:19-24,50-55`, `src/ui/helpers.rs:119-124,147-152` with `text_util::ellipsize(s, N, "…")`.
   - replace `src/ui/helpers.rs:1021-1033` (`truncate_reason`) with `ellipsize` + the word-backoff rule moved inside (fixes the non-boundary panic).
   - `src/manager/context.rs:129-142` → move to `text_util::utf8_safe_slice`, keep a re-export in `context.rs` for existing tests (`src/manager/context_tests.rs`).
   - `src/ui/helpers.rs:157-175` → move body to `text_util::chunk_utf8`; `src/ui/mod.rs:10` re-exports it unchanged.
   - `src/harness/mod.rs:213-231` → `text_util::clip_head_tail(&content, 7000, 2000)`.
   - add `count_text_tokens(&str)`; replace the 9 raw `cl100k_base_singleton()` sites in §2.7.
   - move `src/ui/tui/render.rs:956-964` → `text_util::format_count`, then fix `src/orchestrator/workers.rs:290-294`.
5. `src/tool_args.rs`
   - `src/agents/runner/formatting.rs:8-59`, `:61-174` and `src/ui/helpers.rs:78-156` keep their distinct *output shapes* (log preview vs `name(args)` badge) but must call the shared accessors; this also fixes the `query` vs `pattern` bug (§2.5).
   - `src/harness/sleep.rs:6-18` and `:88-96` → `tool_args::seconds_arg(args)`; the two formatting.rs sleep branches (`:39-48`, `:149-158`) then delete.
   - `src/agents/runner/formatting.rs:79-91` → `tool_args::verdict_pair(args)` sharing `src/agents/validation.rs:51-59` aliases.
6. `src/think_tags.rs`
   - `src/llm/thinking.rs:54-58` stays the definition site; `src/ui/tui/formatting.rs:86-88,107-109` and `:132-138`, and `src/ui/tui/render.rs:130-136,158-164` consume it; extract the duplicated filter in `render.rs:128-141` / `:155-168` into one private `push_streaming_line(&mut Vec<Line>, &str, Style)` helper (stays in `src/ui/tui/render.rs`).
7. `src/ui/bridge/` (no new dependency needed; `sink` and `steer` are siblings)
   - add `src/ui/bridge/arbiter.rs` with `struct SteerSnapshot { plan_content, plan_progress, active_subtasks, history, has_active, effective_msg }`, `fn snapshot(loop_count, …) -> SteerSnapshot`, and `fn apply_decision(&SteerDecision, preempt, …) -> LoopOutcome`.
   - `src/ui/bridge/sink.rs:92-214` and `src/ui/bridge/steer.rs:64-180` then call it; the renderer-vs-channel difference is injected as a `FnMut(&str)` delta callback, so no `agents → ui` edge is introduced (both files stay under `src/ui`).

## 4. Delete-only candidates (never called from production code)

| Item | Location | Evidence |
|---|---|---|
| `load_system_prompt` | `src/ui/helpers.rs:73-76` | `#[allow(dead_code)]`; 0 references anywhere, incl. tests |
| `drain_delegation_events` | `src/ui/helpers.rs:309-316` | `#[allow(dead_code)]`; only caller is `src/ui/session_tests.rs:234` — switch the test to `..._with_transcript(.., None)` |
| `rehydrate_subagents` | `src/ui/helpers.rs:510-518` | `#[allow(dead_code)]`; 7 test-only callers (`src/ui/helpers.rs:1070,1110,1125,1196,1232,1259`); production uses `rehydrate_subagents_with_ui` (`src/ui/session.rs:114`) |
| `centered_rect` | `src/ui/tui/formatting.rs:807-826` | `#[allow(dead_code)]`; only `src/ui/tui/tests.rs:88` |
| `count_single_message_lines` | `src/ui/tui/formatting.rs:511-516` | only `src/ui/tui/tests.rs:143,145`; redundant with `render_message_lines` + `count_wrapped_rendered_lines` |
| `UiTranscript::records_mut` | `src/ui/transcript.rs:60-64` | 0 references in `src/` and `tests/` |
| `format_plan_progress_summary` passthrough | `src/ui/helpers.rs:36-38` (+ re-export `src/ui/mod.rs:10`) | one-line forward to `orchestrator::generate_plan_progress_summary`; inline at `src/ui/session.rs:391`, `src/ui/bridge/sink.rs:101`, `src/ui/bridge/steer.rs:75` |
| Stale XML-rescue doc banner | `src/harness/monitor/repetition.rs:88-103` | describes `xml.rs` rescue; sits on `ToolRepetitionDetector` (C13) |
| `format_tool_args_*` grep branches | `src/agents/runner/formatting.rs:27-30`, `:101-110` | read a non-existent `query` key → always-empty output; fix via `tool_args::arg_pattern` (C6), not a blind delete |
| `#[allow(dead_code)]` on `src/ui/tui/events.rs:376,393,409,424,437` (`cursor_left/right`, `delete_*`, `insert_char`) | out of scope but flagged: same pattern of test-only helpers |

## 5. Largest offenders

1. **C1 task-id strip (31 sites / ~55 duplicated lines)** — highest fan-out; touches `agents`, `harness`, `manager`, `orchestrator`, `ui`.
2. **`src/ui/helpers.rs` (1387 L)** — `rehydrate_subagents_with_ui` `:521-932` (412 L) + `extract_failure_reason` `:933-1019` (87 L, five near-identical “find marker → first non-empty line → `truncate_reason`” blocks) + `format_tool_call_display` `:78-156`.
3. **`src/ui/bridge/sink.rs:81-443` `on_pause` (363 L)** vs `src/ui/bridge/steer.rs:33-301` (C11) — biggest *cross-file* near-duplicate pair.
4. **`src/ui/tui/render.rs` (1107 L)** — `render_subagents:633-954` (322 L), `render_status:976-1088` (113 L), duplicated streaming-line filter (C16).
5. **`src/ui/tui/formatting.rs` (955 L)** — `format_math_expr:211-405` (195 L), `wrapped_lines:527-647` (121 L); plan-line regex forks (C3).
6. **`src/orchestrator/workers.rs:330-454` `get_active_subtasks_str` (125 L)** — active/completed halves are copy-paste twins, incl. the truncation pair C7.
7. **`src/orchestrator/plan_summary.rs:15-141` (127 L)** — third plan parser + internal `trim_start_matches` closure duplicated twice (`:38-47`, `:49-58`).
8. **`src/manager/context.rs:61-142`** — good wrappers that the UI layer bypasses 9× (C8).

Net estimate: unifying C1–C9 removes ≈ 260 duplicated lines and 1 latent panic (`truncate_reason`)
plus 1 always-empty branch (grep preview), with no new `src/agents/* → src/ui/*` edge.
