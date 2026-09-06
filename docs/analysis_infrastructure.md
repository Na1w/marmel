# Deep-Dive Analysis: Infrastructure Subsystems of `marmel` (marmennill v0.6.0)

**Scope:** `src/harness/*`, `src/llm/*`, `src/mcp/*` (including embedded `*_tests.rs` modules)
**Method:** Full read of every in-scope file plus targeted cross-references into `src/main.rs`, `src/types.rs`, `src/config.rs`, `src/orchestrator/mod.rs`, `src/tool_names.rs`.
**Citations:** `file:line` against the workspace tree. No source files were modified.

---

## 0. Executive Summary

`marmel` is an agentic coding assistant whose infrastructure splits into three subsystems:

| Subsystem | Files | Role |
|---|---|---|
| **Harness** | `src/harness/{mod,fs,pty,sandbox,monitor,search,workspace}.rs` (~3,546 LoC) | Tool dispatcher + built-in tool implementations + isolation (path confinement, Landlock, PTY process groups) + loop-resilience monitoring |
| **LLM** | `src/llm/{mod,client,stream,thinking}.rs` (1,629 LoC) | OpenAI-compatible SSE chat client with watchdogs/retries, shared stream channel, thinking-channel demuxer |
| **MCP** | `src/mcp/{mod,client,http}.rs` (945 LoC) | Model Context Protocol client over stdio and HTTP/SSE (JSON-RPC 2.0), tool discovery + invocation |

The architecture is coherent and heavily test-covered (≈1,756 LoC of embedded tests in scope). The strongest layers are the LLM streaming stack (layered watchdogs, resumable turns, steering) and the harness resilience monitor. The weakest points are (a) the **fail-open Landlock sandbox** and the fact that **macOS gets no OS-level sandbox at all**, (b) several **synchronous/blocking bridges into async contexts** (`block_in_place` without runtime-flavor guards), and (c) a handful of **doc/code drift** and **memory-growth** smells detailed in §6.

---

## 1. Harness Layer

### 1.1 `src/harness/mod.rs` — dispatcher and global state (683 lines)

The module doc calls it the "Tool harness: dispatcher and built-in tool implementations" (`src/harness/mod.rs:1`).

**Global state & workspace root resolution**
- Two process-wide statics: `MCP_MANAGER: RwLock<Option<Arc<McpManager>>>` (`src/harness/mod.rs:19-20`) and `WORKSPACE_ROOT: RwLock<Option<PathBuf>>` (`src/harness/mod.rs:21-22`), plus a tokio task-local `SCOPED_WORKSPACE_ROOT` (`src/harness/mod.rs:24-25`).
- `with_workspace_root` (`src/harness/mod.rs:29-36`) scopes a future to a root — used for isolated tests and sub-workspaces; `set_workspace_root` (`:38-45`) sets the global; `get_workspace_root` (`:47-60`) resolves with **task-local > global > CWD** precedence and self-populates the global from CWD on first call (`:52-59`).
- This task-local-first design is how per-specialist workspaces are isolated without passing roots through every signature.

**Data model**
- `ToolInvocation { name, arguments }` (`:76-80`), `ToolResult { content, is_error }` with `ok`/`err` constructors (`:83-101`).
- `HarnessStats` (`:107-175`): atomic counters for resilience interventions (repetition breaks, empty productions, compactions, XML rescues, backend retries, rebirths, steer arbitrations) with `record_*` helpers — the "session telemetry" registry.
- `ToolCaller { Manager, Specialist(Agent) }` (`:148-154`) drives role-based policy; `ToolError` (`:176-184`) has `UnknownTool`, `BadArguments`, `Forbidden`, `Execution(anyhow)`.

**Dispatch pipeline**
- `dispatch` (`:312-368`) is the shared entry point: **MCP tools take precedence over built-ins** (`:313-325` — if the manager has the tool name, the call is bridged via `tokio::task::block_in_place` + `Handle::block_on(mcp.call_tool(...))`), then a large alias table maps legacy/alternate names (`"view_file"`, `"get_file"`, `"bash"`, `"cmd"`, `"pty__spawn"`, …) onto canonical handlers (`:322-352`).
- `dispatch_for` / `dispatch_for_with_engine` (`:435-479`) add caller policy + debug logging + timing, then apply `apply_tool_output_length_limit`.
- `dispatch_manager` (`:481-532`) enforces a **Manager allowlist**: `delegate_task`, `create_plan`, `archive_current_plan`, `rebirth`, `read_file`, `grep_search`, `glob`, `sleep` — everything else returns `ToolError::Forbidden`. Notably the Manager **cannot** `write_file`, `replace`, or `run_command`: a deliberate least-privilege split (Manager orchestrates, specialists mutate).
- `dispatch_specialist` (`:552-660`) forbids `create_plan` for specialists (`:556-561`), applies MCP precedence again (`:563-577`), then gates the (normalized) tool name through `crate::orchestrator::caller_allows_tool` (`:579-586`; implementation at `src/orchestrator/mod.rs:702`) against the per-specialist allowlist table, and finally executes.
- `normalize_tool_name` (`:534-550`) canonicalizes aliases before the policy gate so e.g. `"edit_file"` is gated as `replace`.

**Output hygiene**
- `MAX_TOOL_OUTPUT_CHARS = 10_000` (`:398`); `apply_tool_output_length_limit` (`:400-432`) truncates to head 7,000 + tail 2,000 chars with char-boundary-safe slicing and an explicit `[... TRUNCATED n CHARACTERS ...]` marker. Outputs containing `# EXECUTION PLAN` / `IMPLEMENTATION PLAN` are exempt (`:402-405`) so the orchestrator can read plans verbatim.
- `handle_sleep` (`:229-309`) caps sleeps at 300 s, checks the global cancellation token, and on multi-thread runtimes uses `block_in_place` + `select!` on `sleep` vs `cancelled()`; on current-thread runtimes it degrades to a 50 ms polling loop (`:283-306`).

**How this isolates agent tool execution:** the harness is the single choke point through which every LLM-emitted tool call flows (`dispatch_for_with_engine`), so policy (role allowlists), confinement (`resolve_safe_path` inside each handler), output truncation, repetition blocking, and logging are applied uniformly regardless of which agent calls what.

### 1.2 `src/harness/fs.rs` — filesystem tools (345 lines)

Implements `read_file`, `replace`, `write_file` (REQ-TOOL-002/003/004 per header, `src/harness/fs.rs:1-14`).

- **Path mapping:** `WORKSPACE_PREFIX = "/home/coder/workspace"` (`:16-17`); `map_path` (`:23-33`) rewrites that container prefix onto the workspace root — a compatibility shim for prompts written against a container layout.
- **Path confinement:** `resolve_safe_path` (`:37-89`) is the security core:
  - Builds the raw target from the workspace root (prefix-stripped), absolute paths pass through, relative paths join the root (`:40-49`).
  - Existing targets are `canonicalize()`d (resolves symlinks and `..`); non-existing targets are resolved by walking up to the nearest existing ancestor, canonicalizing it, and re-appending the missing components (`:60-79`).
  - Final check: the canonical target must start with the canonical workspace root **or** the canonical temp dir, else `ToolError::Forbidden { "access denied: path ... escapes workspace root" }` (`:81-88`).
- **`read_file`** (`:105-133`): character-paginated window (default 4,000; clamped to `[2000, 8000]`, constants at `:95-97`) with a `[Showing characters X-Y of Z. Use offset=Y ...]` footer. The clamp floor exists specifically to stop models stuck in micro-pagination loops (doc `:99-100`). Reads the whole file and slices by `chars()` — UTF-8-safe (multibyte tests in `src/harness/fs_tests.rs:151-186`).
- **`replace`** (`:135-175`): strict single-match semantics — 0 matches → error, ≥2 matches → "ambiguous" error, never writes (`:141-153`); on success performs an **atomic write**: temp file `.name.tmp.<pid>` in the same directory, then `rename` (`:156-166`).
- **`write_file`** (`:177-195`): creates missing parents via `create_dir_all` and forces `0o755` on them (`:197-203`, unix-only).
- **Argument coercion:** `str_arg` (`:206-322`) accepts a large alias table per key (`file_path`, `filepath`, `contents`, `text`, `code`, …) plus a heuristic that scrapes a path out of `saved to \`path\`` text inside `content` when `path` is missing (`:231-247`). `usize_arg` (`:324-338`) handles optional integers.

### 1.3 `src/harness/pty.rs` — pseudo-terminal execution (682 lines)

Two distinct execution facilities share this file:

**A. One-shot `run_command` (REQ-TOOL-001)**
- Every command is wrapped as `sh -c "stty -echo 2>/dev/null || true; ulimit -f 4194304 2>/dev/null || ulimit -f 2097152 2>/dev/null; <command>"` (`src/harness/pty.rs:93`; constants `ULIMIT_FILE_BLOCKS`/`_FALLBACK` at `:37-41` = 2 GiB / 1 GiB file-size caps in 512-byte blocks). `stty -echo` keeps the echoed command line out of captured stdout.
- `build_sandboxed_command` (`:74-118`): on Linux, when the running executable is `marmel`, the command is **re-executed through the marmel binary itself** with `--internal-sandbox-exec <cwd> <wrapped>` (`:100-109`) so the child first applies Landlock (see §1.4 and `src/main.rs:22-26`); otherwise plain `sh -c`. Windows gets `cmd /C` wrapping (`:77-91`).
- `PtySession::spawn` (`:121-159`) opens a 24×80 PTY, spawns the command with cwd = workspace root, drops the slave, and captures the **process-group leader pid** via `master.process_group_leader()` (`:129-135`).
- `run_command_pty` (`:209-252`): clones the PTY reader, drains output on a dedicated `std::thread` into an `mpsc` channel, waits with `recv_timeout`, and on timeout tears down and returns `"[command timed out after Xs and was killed]"`. `teardown` (`:166-171`) always runs afterwards (success *and* timeout): `kill_process_group` (SIGKILL to `-pid`, i.e. the whole group, `:659-673`, tolerating `ESRCH`) plus `child.kill()`. This is what guarantees no orphaned subshells/debuggers/REPLs — verified by `test_harness_pty_process_group_kill` (`src/harness/pty_tests.rs:9-56`).
- `sanitize_terminal_output` (`:51-63`) strips OSC sequences (`ESC ] n ; ... BEL/ESC \`), BEL, and backspace, while preserving CSI color codes, `\n`, `\r`, `\t` — tested in `pty_tests.rs:70-86`.

**B. Interactive multi-turn PTY manager**
- `SharedBuffer` (`:259-265`) accumulates output with a read cursor and `last_activity`; `InteractivePtySession` (`:267-274`) holds writer + buffer + master + child; its `Drop` impl (`:276-288`) issues `libc::kill(-pid, SIGKILL)` then `libc::kill(pid, SIGKILL)` inside an `unsafe` block (edition-2024 `unsafe_op_in_unsafe_fn` note at `:22-24`).
- `PtyManager` (`:290-529`) keeps sessions in a `tokio::sync::Mutex<HashMap>`; a background reaper task ticks every 30 s and reaps sessions idle > 300 s (`:305-320`). `spawn` (`:338-431`) starts a reader thread that appends into the shared buffer, waits a fixed **300 ms** (`:421`) to collect banner output, and returns the delta; `write` (`:433-475`) writes input then sleeps `wait_ms` (default 300) and returns new output + liveness; `read` (`:477-504`) and `close` (`:506-510`) follow the same pattern; `list` (`:512-529`) reports pid/alive/idle/bytes.
- `GLOBAL_PTY_MANAGER: LazyLock<PtyManager>` (`:531-532`); the tool handlers `pty_spawn/write/read/close/list` (`:535-652`) are synchronous and bridge into the async manager via `block_in_place` + `block_on` (e.g. `:557-561`).

### 1.4 `src/harness/sandbox.rs` — Landlock LSM (127 lines)

- `apply_sandbox` (`src/harness/sandbox.rs:24-34`) is a no-op off Linux.
- `apply_landlock_linux` (`:37-121`) builds a ruleset at **`ABI::V1`** (`:38`) and grants:
  1. workspace root — full read/write/execute/create/delete (`:53-58`);
  2. `/tmp` — full access (`:61-67`);
  3. `~/.cargo` and `~/.cache` — full access (so cargo/pip/npm can build; `:69-86`);
  4. `~/.rustup` — read-only (`:87-93`);
  5. `/usr /bin /lib /lib64 /opt /etc /dev /proc /sys` — read-only (`:96-107`);
  then `restrict_self()` (`:110-119`). Everything else (including `$HOME` generally, `~/.ssh`, `~/.gnupg`, other projects) is denied.
- **Fail-open semantics:** if the kernel lacks Landlock the function logs a warning and returns `Ok(())` (`:43-49`); if `restrict_self` fails it also logs and returns `Ok(())` (`:112-120`). The sandbox is applied in the re-exec'd child (`src/main.rs:22-26`), not in the parent agent process.

### 1.5 `src/harness/monitor.rs` — resilience monitor (1,239 lines)

The "keeps-the-loop-productive" harness (REQ-HARN-001…004, header `:1-17`):

- **`XMLToolRescue`** (`src/harness/monitor.rs:157-247` with helpers `:250-470`): scans assistant text for tool calls emitted as plain-text XML and rebuilds structured `ToolCall`s with synthetic ids `call_text_{uuid}` (`make_rescued_call`, `:1022-1032`). Three encodings are supported: embedded JSON inside an XML tool-call element (`try_embedded_json`, `:846-865`), a `function="name"` attribute form whose body is heuristically mapped to `content`/`path`/`command`/`query`/`pattern` arguments (`try_function_attr`, `:867-916`), and the legacy `tool_call <function=…><parameter=…>` SPEC form (`try_legacy_function_block`, `:918-958`). Attribute extraction (`extract_attribute`, `:1000-1015`) is deliberately loose.
- **`ToolRepetitionDetector`** (`:477-597`): sliding buffer of the last **50** tool calls (`TOOL_BUFFER_CAPACITY`, `:25`). Equality is *semantic* — JSON key order is ignored (`semantic_json_value_eq`, `:91-94`) and pagination keys (`offset`, `page`) are stripped (`strip_pagination`, `:120-133`) for `read_file`/`grep_search` (`is_pagination_tool`, `:98-101`), so paginated progress is not punished. `detect_consecutive` (`:545-558`) blocks at `threshold` identical consecutive calls; `detect_cycle` (`:565-597`) cuts alternating A→B cycles after `threshold` full cycles. Thresholds are config-driven (`MonitoringConfig`, `src/config.rs:37-55`, default `repetition_threshold = 5`).
- **`RepetitionDetector`** (`:605-760`): rolling char buffer (`TEXT_BUFFER_CAPACITY = 16384`, `:30` — note the doc comments still say "1000-character", `:12`, `:606`, `:640`, `:1100`, `:1195`). Fires when a pattern of length ≥ `min_pattern_len` repeats ≥ `threshold` times at the tail (`tail_repeats`, `:668-695`) or when lines/bigrams/word-4-grams repeat (`line_or_phrase_repeats`, `:703-760`). Extensive false-positive suppression for code: `is_code_pattern` (`:762-790`), `is_code_line` (`:792-880`), `is_code_word` (`:882-960`), `is_markdown_divider` (`:982-992`) — e.g. repeated `return Ok(());` lines or matrix-row literals do not trigger (tests `src/harness/monitor_tests.rs:130-176`).
- **`prune_orphan_tool_messages`** (`:995-1010`): drops `role:"tool"` messages whose `tool_call_id` has no matching assistant tool call (protocol repair for providers that reject orphans).
- **`HarnessMonitor`** facade (`:1080-1239`): binds all three detectors to a shared `Arc<HarnessStats>`; `rescue_xml` (`:1161`), `observe_tool` (`:1173-1180`), `intervention_error` (`:1182-1197` — exact SPEC error strings), and `feed_text` (`:1195-1212`), which fires a repetition break **once** (`repetition_fired` latch), resets the buffer, and increments `repetition_breaks`; `reset_text_break` (`:1221-1224`) re-arms after a stream restart.

**Isolation relevance:** the monitor stops runaway agents (identical-call loops, alternating ping-pong, degenerate text loops) *before* they burn tokens or hammer the filesystem. It is invoked from the agent runners (`src/agents/runner.rs:439,466,709`, `src/agents/validation.rs:192,286`) and the LLM stream layer (`src/llm/stream.rs:388-397,683`).

### 1.6 `src/harness/search.rs` — code search (192 lines)

- `grep_search` (`src/harness/search.rs:24-63`): regex search honoring `.gitignore` via the `ignore` crate (`WalkBuilder::new(&safe_root).require_git(false)`, `:36-38`); the search root passes through `resolve_safe_path` (`:27`); results formatted `path:line: text`, default 100, hard cap `GREP_HARD_CAP = 500` (`:15`, `:28`), early-returning at the cap.
- `glob` / `glob_in_root` (`:67-101`): walks the workspace root, matches relative paths against a glob translated to an anchored regex (`glob_to_regex`, `:104-125`; `**` → `.*`, `**/` → `(?:.*/)?`), sorted, truncated to `GLOB_HARD_CAP = 500` (`:17`, `:99`).
- Both walk non-git trees too (`require_git(false)`), still respecting `.gitignore` files — verified by `test_harness_grep_gitignore` (`search.rs:137-160`).

### 1.7 `src/harness/workspace.rs` — `.marmel` directory owner (173 lines)

- `Workspace` (`src/harness/workspace.rs:24-37`) centralizes the `.marmel` state directory (plan file, `marmel.log`, forced-phase override, `archive/`; constants `:14-21`), replacing scattered `create_dir_all(".marmel")` calls (CODE_REVIEW note `:3-10`).
- `ensure_writable` (`:73-86`) creates the dir and validates writability with a probe file `.marmel_probe_<pid>` that is written then removed — fail-fast at boot on read-only filesystems; the test asserts no probe leftovers (`:127-152`).
- `backup_path` (`:89-95`) and `rotate_log_file` (`:98-117`) implement size-triggered numbered log rotation (`marmel.log.1`, `.2`, …).

### 1.8 How the harness isolates agent tool execution (synthesis)

Isolation is **layered**, each layer owned by a different file:

1. **Policy layer** — role allowlists (`dispatch_manager`/`dispatch_specialist`, `src/harness/mod.rs:481-660`) plus `caller_allows_tool` gating; the Manager cannot mutate or execute.
2. **Path confinement** — every fs/search handler funnels through `resolve_safe_path` (`src/harness/fs.rs:37-89`); canonicalization defeats `..` and symlink escapes; only workspace-root and `/tmp` are reachable.
3. **Process isolation** — PTY spawn with cwd pinned to the workspace root (`src/harness/pty.rs:130-131`), `ulimit -f` write caps, strict timeouts (default 60 s, max 300 s, `:33,198-201`), and unconditional **process-group SIGKILL** teardown (`:166-171,659-673`).
4. **Kernel sandbox (Linux only)** — Landlock re-exec confinement (`src/harness/pty.rs:100-109` → `src/main.rs:22-26` → `src/harness/sandbox.rs`).
5. **Output hygiene** — OSC/control-char sanitization (`src/harness/pty.rs:51-63`), 10k-char tool-output truncation (`src/harness/mod.rs:398-432`), result caps in search (`src/harness/search.rs:15,17`).
6. **Behavioral isolation** — repetition/cycle blocking and text-loop breaks (`src/harness/monitor.rs`), orphan-tool-message pruning.

---

## 2. LLM Layer

### 2.1 `src/llm/client.rs` — API abstraction (508 lines)

**Provider/endpoint model.** There is a single `ChatClient` (`src/llm/client.rs:67-73`) bound to `backend_url + auth_token + model`. It speaks the **OpenAI Chat Completions wire protocol** (`POST {backend}/chat/completions`, `:192-195`) with `stream: true` forced (`:197-201`) and optional `Authorization: Bearer` (`:219-221`). There is no provider-specific SDK: any OpenAI-compatible gateway works, including reasoning-token backends — `ChatChunk.delta.reasoning_content` (aliased as `reasoning`, `src/types.rs:585-588`) and the non-standard `enable_thinking` request flag (`src/types.rs:102-104`) are first-class. Constructors: `from_config` (`:107-114`), `new` (no auth), `new_with_token` (`:116-136`), and `with_initial_timeout_secs` for tests (`:138-142`).

**Watchdogs (three layers, `:11-19`):**
- `INITIAL_RESPONSE_WATCHDOG_SECS = 300` — first SSE event must arrive within 5 min (long prefill); enforced both pre-headers (`:224-261`, polling `send()` in 50 ms slices so `on_delta("")` can abort) and pre-first-event (`:279-306`).
- `INTER_CHUNK_WATCHDOG_SECS = 60` — max silent pause between chunks (`:351`).
- `OVERALL_READ_TIMEOUT_SECS = 1200` — hard cap on the whole streamed read, wrapping the consume loop in `tokio::time::timeout` (`:384-386`).
- Connect timeout 10 s on the per-call `reqwest::Client` (`:187-190`).

**Retry policy.** `ChatError` (`:75-89`) classifies failures; `is_retryable` (`:91-101`) covers HTTP **503/429/502/504**, initial/stall/read timeouts, transport and SSE stream errors. `chat_stream` (`:155-176`) retries up to `MAX_ATTEMPTS = 3` with **linear backoff** `BACKOFF_BASE_MS × attempt` (1 s, 2 s; `:19`, `:166-170`). Non-retryable 4xx (400/401/403) fail fast. Verified by wiremock tests: `test_llm_retry_backoff` (503→429→200, `src/llm/client_tests.rs:31-64`), `test_llm_retry_exhaustion` (`:67-81`), and initial-timeout retry (`:84-115`).

**Streaming protocol & tool-call parsing.** The response body is parsed as SSE via `resp.bytes_stream().eventsource()` (`:276`). `consume_event` (`:429-505`):
- `data: [DONE]` terminates (`:437-443`).
- `delta.reasoning_content` accumulates into `reasoning` and toggles an `in_reasoning` flag that emits `` markers to the `on_delta` callback (`:447-461`).
- `delta.content` closes any open reasoning span and accumulates (`:462-477`).
- `delta.tool_calls` fragments are accumulated in `BTreeMap<usize, (Option<id>, name, arguments)>` keyed by stream index, appending name/argument fragments (`:478-500`); `map_to_tool_calls` (`:418-427`) materializes them, synthesizing `call_{uuid}` ids when the provider sends none.

**Token accounting.** Global `AtomicU64` counters (`:21-34`): input tokens counted from the request (`:213-216`), output tokens estimated with `tiktoken-rs` `cl100k_base_singleton` over content + reasoning + per-tool-call overhead (`count_reply_tokens`, `:39-51`). A unit test pins that chunk-level `consume_event` does **not** double-count (`client_tests.rs:143-172`).

### 2.2 `src/llm/stream.rs` — shared stream channel (766 lines)

The routing layer between the raw SSE client and the UI/orchestrator:

- **Events & control:** `StreamEvent::{Content, Thinking, Status}` (`src/llm/stream.rs:14-22`); `StreamControl::{Continue, Abort, Pause{user_input}}` (`:25-33`); `PauseAction::{Resume, Abort}` (`:36-41`). The `StreamSink` trait (`:45-70`) has `emit`, `is_aborted`, `poll_control` (polled on delta boundaries), and async `on_pause`. `NullSink` (`:73-78`) and `VecSink` (`:82-99`) serve tests/transcripts.
- **`TurnStreamHandler`** (`:181-341`) is the per-turn state machine: token budget (`max_tokens.max(256)`, `:222`), repetition-detector wiring, `ThinkingDemuxer`, optional cancellation token, and pause capture. `on_chunk`/`on_chunk_with_sink` (`:236-315`) push deltas through the demuxer into the repetition detector and either orchestrator events (`Event::Message`/`Event::Thinking`) or the sink; they return `false` (cut stream) on cancellation, repetition trigger, budget exceedance, `Abort`, or `Pause`. `finish`/`finish_with_sink` flush pending demux state; `into_message` yields the final assistant `Message`.
- **`drive_streamed_turn`** (`:343-414`): a generic turn loop over any `chat` closure — demuxes `reply.raw` through a fresh demuxer, extends tool calls, performs **XML tool rescue** when there are no structured calls (`:388-397`), and nudges empty productions via `NudgePolicy` (`:399-410`).
- **`build_request`** (`:426-445`): assembles `ChatRequest` from `StreamConfig` (model, temperature/top-p/penalties, `stream: true`) with `ToolDef::manager_tools()` plus MCP tools filtered to `cfg.mcp_servers` (`:431-435`) — this is how MCP tools reach the model's tool list.
- **Resumable streaming with steering:** `chat_stream_resumable` (`:504-614`) supports **mid-flight pause**: when the sink requests `Pause`, the handler records the user input and the sink's `on_pause` decides Resume vs Abort; on Resume a **continuation request** is built by appending the accumulated assistant content *and* thinking as a prefill assistant message (`build_continuation_request`, `:447-472`), so generation resumes exactly where it stopped. On a mid-stream error after partial output, a **fallback continuation** (`:474-493`) appends the partial content plus a system continuation notice and retries once (`:527-546`). `ResumableStreamOutput` (`:495-502`) reports `budget_exceeded`, `rep_triggered`, `was_aborted_by_steer`.
- **`chat_client_turn`** (`:616-766`): the full turn loop — applies one-turn `recovery` adjustments, runs `chat_stream_resumable`, performs XML rescue (`:683`), and injects three kinds of recovery nudges as synthetic transcript messages: **budget-exceeded** (`:705-737`), **repetition-loop** (`:739-769`), and **empty-production** (`:771-782`), each bounded by `NudgePolicy::max_attempts` (3) and flipping `recovery = true` for the next attempt.

### 2.2.1 Retry/timeout/error-handling summary

| Mechanism | Constant / site | Behavior |
|---|---|---|
| First-byte watchdog | 300 s (`client.rs:11`, enforced `:224-261`, `:279-306`) | `ChatError::InitialTimeout`, retryable |
| Inter-chunk stall watchdog | 60 s (`client.rs:13`, `:351`) | `ChatError::StallTimeout`, retryable |
| Overall read cap | 1200 s (`client.rs:15`, `:384-386`) | `ChatError::ReadTimeout`, retryable |
| Connect timeout | 10 s (`client.rs:188`) | transport error, retryable |
| Retry attempts | 3 (`client.rs:17`) with 1 s·attempt backoff (`:166-170`) | only retryable classes |
| Cooperative abort | `on_delta` returning false (`:227-232`, `:284-288`, `:313-316`) | returns empty `StreamedReply` silently |
| Turn-level recovery | `apply_recovery` one-turn suppression (`thinking.rs:271-283`) | `enable_thinking=false`, `frequency_penalty +0.5`, `temperature +0.1` |
| Mid-stream error fallback | `chat_stream_resumable` (`stream.rs:527-546`) | continuation prefill retry, then original error |

### 2.3 `src/llm/thinking.rs` — reasoning-token handling (337 lines)

- **Tag demuxing (REQ-LLM-002):** `ThinkingDemuxer` (`src/llm/thinking.rs:43-52`) splits a raw stream into a visible `content` channel and a `thinking` channel using three tag pairs: ``, `` (`TAG_PAIRS`, `:54-57`). `push_delta` (`:82-156`) is incremental and **delta-boundary-safe**: a `pending` buffer holds text that might be a partial tag, and `partial_prefix_split` (`:239-253`) finds the longest proper-tag prefix at the tail — char-boundary safe for multibyte/emoji (tests `src/llm/thinking_tests.rs:64-92`). Tags are consumed as state and stripped from the payload unless `preserve_thinking` is set, in which case both tags and thinking text are duplicated into content (`append`, `:174-188`). `into_message` (`:214-229`) maps the channels to `Message::Assistant { content, reasoning_content }`.
- **Recovery policy (REQ-LLM-003):** `apply_recovery` (`:271-283`) builds a one-turn mutated copy: `enable_thinking = Some(false)`, `frequency_penalty += 0.5`, `temperature += 0.1` (defaults in `RecoveryAdjustment`, `:23-34`). Callers own the one-turn semantics; the stream loop does exactly that (`stream.rs:354-361`, `:630-636`).
- **Empty-production nudging (REQ-LLM-004):** `NudgePolicy` (`:286-327`) — max 3 attempts, nudge text `"?"` appended as a user message; `should_nudge(attempts_used)` is 0-indexed and pre-check.

Note the two-layer thinking design: providers that emit a *structured* `reasoning_content` field are demuxed in `client.rs::consume_event`; providers that inline `…` tags in the text stream are demuxed by `ThinkingDemuxer` in `stream.rs`/`thinking.rs`. Both feed the same `reasoning_content` message field.

---

## 3. MCP Layer

### 3.1 `src/mcp/mod.rs` (12 lines)

Declares the subsystem: "Model Context Protocol (MCP) Client implementation … JSON-RPC 2.0 communication over stdio and HTTP/SSE transports, supporting server initialization, tool discovery (`tools/list`), and tool execution (`tools/call`)" (`src/mcp/mod.rs:1-3`). Re-exports `McpClient`, `McpManager`, `McpServerConfig`, `McpTool`, `HttpSseConnection` (`:9-12`).

### 3.2 `src/mcp/client.rs` — stdio transport + manager (574 lines)

- **Config & tool model:** `McpServerConfig` (`src/mcp/client.rs:19-28`) holds `command`/`args`/`env` (stdio) or `url` (HTTP). `McpTool` (`:32-43`) carries `name`, `description`, `input_schema`, `server_name`; `qualified_name()` (`:46-48`) returns `server__tool`, guaranteeing uniqueness across servers that expose identically-named tools (tested at `:462-477`).
- **Stdio transport:** `StdioMcpConnection` (`:52-58`) spawns the server process with piped stdio/stdout/stderr (`:61-112`); stderr is drained to `tracing::debug!` on a side task (`:74-82`). The JSON-RPC handshake sends `initialize` (protocol version `2024-11-05`, `clientInfo: marmel`, `:199-215`) followed by the `notifications/initialized` notification (`:185-197`).
- **Request/response loop:** `send_request` (`:114-183`) writes newline-delimited JSON-RPC, then reads lines with a **30 s timeout per read** (`:128-133`), skipping empty lines and mismatched ids (`:143-146`), and surfaces JSON-RPC errors via `JsonRpcResponse::into_result`. `list_tools` (`:217-241`) parses `tools` entries (falling back to `{"type":"object"}` when `inputSchema` is missing/null); `call_tool` (`:243-276`) sends `tools/call {name, arguments}` and flattens the `content` array (text items concatenated; non-text items JSON-serialized), honoring the `isError` flag (`:266-271`). `shutdown` (`:278-281`) kills the child.
- **`McpClient` enum** (`:284-355`): `Stdio(Box<Mutex<StdioMcpConnection>>)`, `HttpSse(Mutex<HttpSseConnection>)`, plus a `#[cfg(test)] Mock` used by the routing tests (`:294-298`, `:318-336`).
- **`McpManager`** (`:358-443`): `boot` (`:369-405`) iterates configured servers — stdio if `command` is set, HTTP/SSE if `url` is set — connects, discovers tools, and indexes them by qualified name; per-server failures are logged as warnings and skipped (fail-soft). `tools_for_servers` (`:413-422`) filters the advertised tool list by server (empty list ⇒ no MCP tools advertised); `has_tool` (`:424-426`) and `call_tool` (`:428-438`) resolve the qualified name and dispatch the **raw** tool name to the owning server (routing verified by the mock test at `:480-505`). `shutdown` (`:440-443`) closes all clients.

### 3.3 `src/mcp/http.rs` — HTTP/SSE transport (359 lines)

Implements the MCP **"Streamable HTTP"** transport (header `src/mcp/http.rs:1-8`):

- `HttpSseConnection` (`:83-91`) holds the endpoint, a `reqwest::Client` with a 30 s timeout (`:101-103`), an optional `Mcp-Session-Id`, and a `request_timeout` of 30 s (`:111`).
- `post` (`:119-184`): POSTs the JSON-RPC request with `Accept: application/json, text/event-stream` and echoes the `Mcp-Session-Id` header once captured (`:131-137`, `:146-149`). If the response is `text/event-stream`, `read_sse_response` (`:186-213`) parses events until one carries the matching id (each SSE event guarded by the 30 s timeout, `:194-199`); otherwise the body is parsed as a single JSON-RPC response with HTTP status checking (`:215-229`).
- Notifications (`:227-266`) are POSTed without an id; session-id capture is repeated.
- `initialize` (`:270-287`) mirrors the stdio handshake; `list_tools` (`:289-315`) and `call_tool` (`:318-354`) mirror the stdio parsing logic. `shutdown` (`:356-359`) is a no-op (the connection is dropped).
- Shared JSON-RPC envelopes live here: `JsonRpcRequest` (`:22-28`), `JsonRpcNotification` (`:32-37`), `JsonRpcResponse` with `id_matches` (`:52-59`) and `into_result` (`:61-72`), `JsonRpcError` (`:76-80`). Serialization/id-matching are unit-tested in `src/mcp/http_tests.rs:11-58`.

### 3.4 Tool discovery & invocation flow (end-to-end)

1. **Boot:** `main.rs` builds the tokio runtime and calls `McpManager::boot(&cfg.mcp_servers)` (`src/main.rs:63-67`), then registers it globally via `harness::set_mcp_manager` (`:66`).
2. **Discovery:** each connection performs the `initialize` handshake, then `tools/list`; tools are stored under `server__tool` qualified names (`src/mcp/client.rs:369-405`).
3. **Advertising:** `StreamConfig::from_config` carries `orchestration.mcp_servers`; `build_request` merges `mcp.tools_for_servers(...)` into the model's tool list as `ToolDef::from_mcp` (qualified names; `src/llm/stream.rs:426-445`, `src/types.rs:520`).
4. **Invocation:** when the model calls a qualified MCP tool, the harness dispatcher checks `mcp.has_tool(name)` **before** built-in handling (`src/harness/mod.rs:313-325, 488-498, 563-577`) and bridges the async `call_tool` through `block_in_place`/`block_on`.
5. **Result normalization:** `content[]` text items are concatenated; `isError: true` becomes an `Err` surfaced as `ToolResult::err("MCP tool error: …")` (`src/harness/mod.rs:318-324`).

---

## 4. Security Posture

### 4.1 What the sandbox actually restricts (`src/harness/sandbox.rs`)

- **Grant set (Linux, ABI V1):** workspace root (RWX+create/delete), `/tmp` (full), `~/.cargo` + `~/.cache` (full), `~/.rustup` (read), `/usr /bin /lib /lib64 /opt /etc /dev /proc /sys` (read-only) (`sandbox.rs:53-107`). All other paths — including the rest of `$HOME`, `~/.ssh`, `~/.gnupg`, sibling projects — are denied once `restrict_self()` succeeds (`:110-119`).
- **Residual risks:**
  - `~/.cargo` and `~/.cache` are writable by design (build tooling) — a malicious command can poison build caches or stash exfiltrated data there (`:69-86`).
  - `/etc` is world-readable in the ruleset (`:97-107`), so readable secrets under `/etc` (e.g. configs with credentials) are exposed to sandboxed commands.
  - `/proc` read-only grants introspection of host processes.
- **Fail-open behavior:** unsupported kernel or `restrict_self` failure → warning + continue unsandboxed (`:43-49`, `:112-120`). There is no strict mode.
- **Coverage gaps:** the sandbox applies only to the re-exec'd child of `run_command`/`pty_spawn` on Linux (`src/harness/pty.rs:100-109` → `src/main.rs:22-26`); the parent marmel process itself is never confined, and on **macOS (the development platform here) or Windows there is no OS-level confinement at all** — only the userspace `resolve_safe_path` checks apply. The re-exec is also skipped when the binary is not named `marmel` (`pty.rs:100-104`).
- Landlock `ABI::V1` (`:38`) does not handle the `Refer` (v2) or `Truncate` (v3) access rights; the `landlock` crate supports higher ABIs. Practical impact is limited (renames/truncates still require write access to the destination), but file-rename-based escapes are governed only indirectly.

### 4.2 Path traversal protections (`fs.rs` / `workspace.rs`)

- `resolve_safe_path` (`src/harness/fs.rs:37-89`) canonicalizes existing targets (defeating `..` and symlinks) and canonicalizes the nearest existing ancestor for not-yet-existing targets, then enforces a prefix check against the canonical workspace root or temp dir (`:81-88`). Tested for `../../etc/passwd` and `/root/.ssh/id_rsa` escapes (`src/harness/fs_tests.rs:247-272`).
- **TOCTOU window:** the existence check, canonicalization, and the subsequent `std::fs::read/write` are separate operations (`fs.rs:52-59` vs `:111,140,180`); an attacker-controlled symlink swapped in between could still redirect I/O. There is no `O_NOFOLLOW`-style openat hardening.
- **Temp-dir escape hatch:** allowing the canonical temp dir (`fs.rs:39-41,81-88`) means any in-workspace tool can read/write anything under `/tmp` — acceptable for build tooling, but it widens the writable surface beyond the workspace.
- `workspace.rs` confines only marmel's own state directory (`.marmel`); it is not a security boundary, but its probe-file check (`src/harness/workspace.rs:73-86`) fails fast on unwritable state.

### 4.3 Command execution risks (`pty.rs`)

- **Arbitrary command execution is the tool's purpose** — `run_command` executes any string via `sh -c` (`src/harness/pty.rs:93`). The mitigations are confinement (cwd = workspace root, `:130-131`), `ulimit -f` (`:37-41,93`), timeouts with process-group SIGKILL (`:166-171,209-252,659-673`), and (Linux only) Landlock.
- **Network is not restricted**: neither Landlock ABI V1 (filesystem-only) nor any other layer blocks outbound connections, so sandboxed commands can exfiltrate data or download payloads.
- **Environment inheritance:** spawned shells inherit the parent environment (`CommandBuilder` does not scrub it), so secrets in env vars are visible to every command.
- **Interactive PTY sessions** (`pty_spawn/write/read`) are longer-lived by design; the 300 s idle reaper (`:305-320`) bounds leakage, and `Drop` kills the group (`:276-288`), but a session can run arbitrary commands unattended for up to 5 minutes of idleness.
- **Output sanitization** (`:51-63`) prevents terminal-escape injection into the agent's context (OSC title rewrites, bell spam), though CSI sequences are deliberately preserved.

### 4.4 MCP-specific considerations

- MCP tool names are qualified (`server__tool`), and dispatch prefers MCP over built-ins when the name matches (`src/harness/mod.rs:313-325`). Since the LLM only sees advertised MCP tools under qualified names (`src/types.rs:520`), accidental shadowing requires a server exposing a *qualified-looking* name — but a malicious/compromised MCP server controls its own tool names and results, i.e. it is trusted with whatever the model sends as arguments.
- Stdio MCP servers are spawned with `env` from config and unbounded capabilities (no sandbox applied to them); HTTP/SSE servers get a 30 s request timeout (`src/mcp/http.rs:101,111`) and session-id handling but no TLS pinning/auth headers beyond what `url` encodes.

---

## 5. Test Coverage in Scope

Embedded test modules (all compile via `#[cfg(test)] #[path = "..._tests.rs"] mod tests;`):

| Test file | Coverage highlights |
|---|---|
| `src/harness/fs_tests.rs` (300 LoC) | replace uniqueness (0/1/≥2 matches, `:14-64`), pagination footers and limit clamping (`:66-148`), multibyte UTF-8 slicing (`:151-186`), path-confinement sandbox escapes (`:247-272`), `map_path` mapping (`:110-126`), write_file parent creation and path-inference heuristic (`:275-300`) |
| `src/harness/pty_tests.rs` (165 LoC) | process-group kill leaves zero orphans (`:9-56`), shell wrapper preamble (`:59-67`), OSC/bell sanitization (`:70-86`), timeout kill (`:89-102`), interactive manager lifecycle (`:105-165`) |
| `src/harness/monitor_tests.rs` (567 LoC) | semantic JSON equality, pagination exemption (`:0-90`), sliding-buffer cap (`:92-102`), text-repetition thresholds and length bounds (`:104-176`), false-positive suppression on realistic code (`:130-176`), composed monitor stats (`:307-499`) |
| `src/llm/client_tests.rs` (192 LoC) | wiremock-backed retry backoff (503→429→200, `:31-64`), retry exhaustion (`:67-81`), initial-timeout retry (`:84-115`), cooperative abort during prefill (`:118-141`), token double-count guard (`:143-172`) |
| `src/llm/stream_tests.rs` (153 LoC) | thinking demux through the turn loop (`:4-33`), repetition detector integration (`:36-44`), continuation/fallback request builders (`:47-77`), mid-stream pause handling (`:80-153`) |
| `src/llm/thinking_tests.rs` (200 LoC) | tag demux char-by-char (`:22-36`), preserve mode (`:39-47`), recovery adjustments incl. None defaults (`:70-92`), nudge policy (`:95-110`), multibyte/partial-tag safety (`:113-141`), incremental emission (`:144-172`) |
| `src/mcp/http_tests.rs` (179 LoC) | JSON-RPC envelope serialization (`:11-30`), id matching numeric/string (`:33-50`), error surfacing (`:53-73`), tool parsing with null-schema fallback (`:61-107`), `tools/call` result flattening incl. `isError` (`:110-179`) |
| `src/mcp/client.rs` inline tests (`:445-574`) | qualified-name disambiguation (`:462-477`), mock routing chain dispatches raw name (`:480-505`), unknown-tool error (`:507-516`), server filtering (`:518-546`), deterministic iteration (`:548-573`) |

Gaps: no tests exercise `resolve_safe_path` against symlink-based escapes, no integration test for the Landlock path (hard to CI), no test for `HttpSseConnection` against a live SSE endpoint (only envelope logic is covered), and the `--internal-sandbox-exec` re-exec path is untested.

---

## 6. Observations: Strengths, Weaknesses, Unsafe Code, Blocking Calls, Smells

### 6.1 Design strengths

1. **Single dispatch choke point with role-based least privilege.** Every tool call flows through `dispatch_for_with_engine` (`src/harness/mod.rs:435-479`); the Manager physically cannot write files or run commands (`:481-532`), and specialists are gated per allowlist (`:579-586`). This is a clean enforcement of "orchestrators orchestrate, specialists mutate."
2. **Defense-in-depth isolation.** Userspace path confinement (`fs.rs:37-89`) + PTY process-group teardown (`pty.rs:166-171`) + `ulimit -f` (`:93`) + Landlock re-exec (`:100-109`, `main.rs:22-26`) + output sanitization/truncation. Each layer is independently testable, and the process-group kill is regression-tested against orphaned background processes (`pty_tests.rs:9-56`).
3. **Production-grade LLM streaming.** Three watchdog layers (300 s first byte / 60 s stall / 1200 s total, `client.rs:11-19`), retryable-error classification with linear backoff (`:91-101,155-176`), cooperative abort via `on_delta`, and wiremock tests proving the retry ladder (`client_tests.rs:31-115`).
4. **Resumable streaming + steering.** `chat_stream_resumable` (`stream.rs:504-614`) pauses mid-flight on user steering and resumes via assistant-prefill continuation (`:447-472`), preserving both content and thinking channels — an uncommon and well-engineered feature.
5. **Resilience monitor with false-positive engineering.** The repetition detectors explicitly exempt pagination progress (`monitor.rs:98-101,120-133`) and code-shaped text (`:762-992`), with tests pinning realistic-code non-flags (`monitor_tests.rs:130-176`) — the hard part of loop detection done carefully.
6. **UTF-8 correctness throughout.** Char-boundary-safe truncation (`mod.rs:409-419`), char-based pagination (`fs.rs:105-133`), and emoji-safe partial-tag handling in the demuxer (`thinking.rs:239-253`, `thinking_tests.rs:113-141`).
7. **Deterministic MCP routing.** Qualified `server__tool` names prevent cross-server collisions and the manager dispatches the raw name back to the owning server, with mock-based routing tests (`mcp/client.rs:46-48,480-505`).

### 6.2 Weaknesses and risks

1. **Fail-open sandbox (High, security).** Landlock failure → warn and continue (`src/harness/sandbox.rs:43-49,112-120`); macOS and non-Landlock kernels get *zero* OS-level confinement for shell commands; the parent process is never sandboxed. Combined with unrestricted network access (§4.3), the practical isolation of `run_command` on the primary dev platform (macOS) is only cwd + `ulimit -f` + timeouts.
2. **`/etc` readable and `~/.cargo`/`~/.cache` writable inside the sandbox** (`sandbox.rs:69-86,96-107`) — deliberate for build tooling but expands the exfiltration/poisoning surface.
3. **MCP precedence over built-ins** (`src/harness/mod.rs:313-325,488-498,563-577`): a configured MCP server exposing a name that collides with a built-in (after qualification) intercepts the call; combined with `ToolDef::from_mcp` advertising qualified names, this is controlled, but any future change to unqualified MCP names would silently shadow built-ins.
4. **Blocking bridges into async contexts.** The MCP dispatch and all PTY tool handlers use `tokio::task::block_in_place` + `Handle::block_on` (`src/harness/mod.rs:316-318,489-493,568-572`; `pty.rs:557-561,588-590,610-612,630-632,645-647`) *without* the runtime-flavor guard that `handle_sleep` carefully implements (`mod.rs:260-281`). On a current-thread runtime these panic ("can call blocking only when running on the multi-threaded runtime"). The main binary builds a multi-thread runtime (`main.rs:57-59`), but library/test callers may not.
5. **Retry can duplicate emitted deltas.** `chat_stream` retries the whole request after a mid-stream failure (`client.rs:155-176`); deltas already pushed through `on_delta` (and thus into the UI/demuxer) are re-emitted from scratch on the retry, so the visible transcript can contain duplicated prefixes and the repetition detector may see artificial repeats.
6. **Unbounded memory growth in interactive PTY sessions.** `SharedBuffer.output` only ever grows (`pty.rs:259-265`); long-lived sessions (e.g. a dev server) accumulate the full byte history in memory even though only the cursor delta is ever read.
7. **Doc/code drift.** `monitor.rs` repeatedly documents a "1000-character rolling buffer" (`:12,606,640,1100,1195`) while `TEXT_BUFFER_CAPACITY = 16384` (`:30`); duplicated `#[cfg(test)]` attributes (`fs.rs:341-342`, `pty.rs:678-679`, `thinking.rs:333-334`); a duplicated doc line in `stream.rs:170-171`.

### 6.3 Unsafe code

All `unsafe` in scope is confined to `src/harness/pty.rs` and is minimal and well-scoped:
- `kill_process_group`: `unsafe { libc::kill(-pid, SIGKILL) }` (`pty.rs:659-661`) — sound: the pid comes from `process_group_leader()`/`process_id()` of a spawned child; `ESRCH` is tolerated (`:663-668`).
- `InteractivePtySession::drop`: `unsafe { libc::kill(-(pid as i32), SIGKILL); libc::kill(pid as i32, SIGKILL) }` (`:280-283`) — same rationale; runs in `Drop`, so teardown is deterministic.
- Tests also use `libc::kill(pid, 0)` liveness probes (`pty_tests.rs:32,45`).
- The edition-2024 `unsafe_op_in_unsafe_fn` constraint is explicitly acknowledged (`pty.rs:22-24`). No unsafe appears in `fs.rs`, `search.rs`, `sandbox.rs`, `llm/*`, or `mcp/*`.

### 6.3.1 Blocking calls in async contexts

- `block_in_place` + `block_on`: MCP dispatch (3 sites, `mod.rs:316-318,489-493,568-572`), all five PTY handlers (`pty.rs:557-561,588-592,610-614,630-634,645-649`), and `handle_sleep` on multi-thread flavor (`mod.rs:260-281`). Only `handle_sleep` checks `RuntimeFlavor` first; the others assume multi-thread and will panic on current-thread runtimes.
- Synchronous `std::fs` I/O (`fs.rs:111,140-166,180-193`; `search.rs:41-56`; `workspace.rs:73-86`) is called from sync tool handlers — acceptable given the handlers are invoked via `block_in_place`, but it means tool execution occupies a tokio blocking slot for the whole call.
- `PtyManager::new` spawns a background task (`pty.rs:305-320`) and `GLOBAL_PTY_MANAGER` is a `LazyLock` (`:531-532`) — first touch must occur inside a tokio runtime context (true in production; a foot-gun for direct unit use outside `#[tokio::test]`).

### 6.4 Code smells (file:line)

1. **Doc/code drift:** "1000-character" buffer comments vs `TEXT_BUFFER_CAPACITY = 16384` — `src/harness/monitor.rs:12,30,606,640,1100,1195`.
2. **Duplicated `#[cfg(test)]` attributes** — `src/harness/fs.rs:341-342`, `src/harness/pty.rs:678-679`, `src/llm/thinking.rs:333-334`.
3. **Duplicated doc-comment line** — `src/llm/stream.rs:170-171` ("Target destination for demuxed stream events." twice).
4. **Fixed-latency synchronization hack:** `PtyManager::spawn` sleeps a flat 300 ms to collect banner output — `src/harness/pty.rs:421` (and default `wait_ms = 300` in `write`, `:444`).
5. **Unbounded `SharedBuffer.output` growth** — `src/harness/pty.rs:259-265,384-389`: cursor-based reads never trim the vector.
6. **`read_file` is O(file) per page:** the whole file is read and `chars().skip(start)` re-iterates from zero for every page — `src/harness/fs.rs:111-121`; sequential pagination of a large file is quadratic in total characters.
7. **`grep_search` loads entire files into memory** and is single-threaded despite the `ignore` walker supporting parallelism — `src/harness/search.rs:41-56`.
8. **Heuristic argument scraping** (`saved to \`path\``) is fragile prompt-format coupling — `src/harness/fs.rs:231-247` (though it is tested, `fs_tests.rs:275-287`).
9. **Per-call XML-rescue stats are discarded:** `stream.rs` builds `HarnessMonitor::with_new_stats()` ad hoc (`src/llm/stream.rs:390-392,681-683`), so `xml_tool_rescues` increments a throwaway registry instead of the session `HarnessStats` — the counters wired in `agents/runner.rs` are the real ones.
10. **`raw` field asymmetry:** `StreamedReply.raw` accumulates content + reasoning but not tool-call fragments (`client.rs:296-316`), so `drive_streamed_turn`'s re-demux of `raw` (`stream.rs:356-367`) cannot see tool calls — harmless today, but a trap if raw is ever used for reconstruction.
11. **`ChunkToolCall.index` is mandatory** (`src/types.rs:593-595`): providers that omit `index` for single tool calls would fail deserialization of the whole chunk (silently skipped by `serde_json::from_str` in `consume_event`, `client.rs:439`).
12. **`glob_to_regex` fallback swallows errors** into a `^$` (match-nothing) regex — `src/harness/search.rs:123-125`; an invalid pattern silently returns "no matches" rather than an error.
13. **`map_path` is effectively dead in the tool path:** tools use `resolve_safe_path` directly; `map_path` survives only for compatibility/tests (`src/harness/fs.rs:23-33`, referenced only in `fs_tests.rs:110-127`).
14. **Silent `RwLock` poisoning tolerance:** `set_workspace_root`/`set_mcp_manager` ignore lock errors (`src/harness/mod.rs:41-44,64-67`), so a poisoned lock would silently disable MCP dispatch and workspace-root overrides.
15. **`ulimit -f` fallback chain hides platform differences** (`pty.rs:93`): if both limits are rejected the command still runs with the default limit — acceptable, but the doc implies a guaranteed cap.

### 6.5 Recommendations (non-exhaustive)

1. Make sandbox failure configurable (strict = abort the tool call; permissive = warn) and consider a macOS `sandbox-exec`/Seatbelt profile to close the platform gap.
2. Add a runtime-flavor guard (or convert to async handlers) for the MCP/PTY `block_in_place` bridges.
3. Deduplicate retry-emitted deltas: buffer deltas per attempt and only forward them once the stream succeeds, or tag retried streams so the demuxer can reset.
4. Trim `SharedBuffer` on read (drop consumed prefix) or cap total bytes.
5. Align the monitor docs with `TEXT_BUFFER_CAPACITY` and clean the duplicated attributes/lines.
6. Consider `ignore`'s parallel walker for `grep_search` and a `Seek`-based reader for `read_file` pagination.

---

## 7. Source Index (files read in full)

- `src/harness/mod.rs` (683), `src/harness/fs.rs` (345), `src/harness/fs_tests.rs` (300)
- `src/harness/pty.rs` (682), `src/harness/pty_tests.rs` (165)
- `src/harness/sandbox.rs` (127), `src/harness/monitor.rs` (1,239), `src/harness/monitor_tests.rs` (567)
- `src/harness/search.rs` (192), `src/harness/workspace.rs` (173)
- `src/llm/mod.rs` (18), `src/llm/client.rs` (508), `src/llm/client_tests.rs` (192)
- `src/llm/stream.rs` (766), `src/llm/stream_tests.rs` (153)
- `src/llm/thinking.rs` (337), `src/llm/thinking_tests.rs` (200)
- `src/mcp/mod.rs` (12), `src/mcp/client.rs` (574), `src/mcp/http.rs` (359), `src/mcp/http_tests.rs` (179)
- Cross-references: `src/main.rs`, `src/types.rs`, `src/config.rs`, `src/orchestrator/mod.rs`, `src/tool_names.rs`, `Cargo.toml`

*Report generated from static analysis of the workspace tree; no source files were modified.*