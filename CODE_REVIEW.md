# Marmennill (marmel) Code Review & Bug Audit

**Date:** September 2026  
**Status:** 241 unit/integration tests passing  
**Scope:** Core library (`src/`), harness execution, orchestrator, specialist subagents, TUI, and LLM streaming pipeline.

---

## Executive Summary

The Marmennill codebase is well-structured, follows idiomatic Rust, and is backed by an extensive test suite. However, an in-depth audit of execution paths reveals several **critical runtime failure modes**, **concurrency race conditions**, and **architectural divergences** between the test harness and the interactive runtime.

---

## 1. Critical Bugs & Runtime Hazards

### 1.1 Runtime Panic on Fractal Sub-Delegation (`block_on` in Async Context)
* **Location:** [`src/agents/runner.rs:533-537`](file:///home/fredrik/marmel/wip/src/agents/runner.rs#L533-L537) & [`src/orchestrator/mod.rs:610-614`](file:///home/fredrik/marmel/wip/src/orchestrator/mod.rs#L610-L614)
* **Description:**
  [`run_specialist_live`](file:///home/fredrik/marmel/wip/src/agents/runner.rs#L243) is an `async fn` executing on a Tokio worker thread. When a specialist invokes a tool, it calls [`dispatch_for_with_engine`](file:///home/fredrik/marmel/wip/src/harness/mod.rs#L356) synchronously on the current worker thread.
  If the specialist (e.g. `Coder`) calls `delegate_task` to sub-delegate work to `Validator` or `Generalist`:
  ```rust
  let deliverable = if let Ok(handle) = tokio::runtime::Handle::try_current() {
      handle.block_on(manager.delegate(req))
  }
  ```
  Calling `handle.block_on(...)` while running on an asynchronous Tokio thread immediately panics:
  > *"Cannot start a runtime from within a runtime. This happens because a function (like block_on) attempted to block the current thread while the thread is being used to drive asynchronous tasks."*
* **Remediation:**
  Wrap the synchronous delegation execution in `tokio::task::block_in_place(|| handle.block_on(...))` or execute tool dispatches from specialists inside `tokio::task::spawn_blocking`.

---

### 1.2 Silent Tool Dropping in `AgentLoop`
* **Location:** [`src/manager/loop.rs:379-391`](file:///home/fredrik/marmel/wip/src/manager/loop.rs#L379-L391)
* **Description:**
  When `AgentLoop` partitions tools during [`run_turn`](file:///home/fredrik/marmel/wip/src/manager/loop.rs#L302):
  ```rust
  let reads: Vec<_> = tools.iter().filter(|t| is_read_tool(&t.invocation.name)).cloned().collect();
  let writes: Vec<_> = tools.iter().filter(|t| is_write_tool(&t.invocation.name)).cloned().collect();
  ```
  - `is_read_tool` matches only: `read_file`, `grep_search`, `glob`.
  - `is_write_tool` matches only: `delegate_task`, `write_file`, `replace`, `run_command`.
  
  Any tool outside these sets is silently dropped and never executed. This affects:
  - `create_plan`
  - `archive_current_plan`
  - `rebirth`
  - `leave_verdict`
  - All interactive PTY tools (`pty_spawn`, `pty_write`, `pty_read`, `pty_close`, `pty_list`)
  - All dynamic **MCP server tools**
* **Remediation:**
  Do not discard unclassified tools. Treat any tool where `!is_read_tool(...)` as a sequential execution tool in `writes`.

---

### 1.3 `delegate_task` Concurrency Race Condition Corrupting `CrashJournal`
* **Location:** [`src/ui/session.rs:411-416`](file:///home/fredrik/marmel/wip/src/ui/session.rs#L411-L416) vs [`src/orchestrator/freeze.rs:96-103, 142-145`](file:///home/fredrik/marmel/wip/src/orchestrator/freeze.rs#L96-L103)
* **Description:**
  In [`ui/session.rs`](file:///home/fredrik/marmel/wip/src/ui/session.rs#L411), `delegate_task` is categorized as parallel:
  ```rust
  let all_parallel = tool_calls.iter().all(|c| {
      matches!(c.function.name.as_str(), "delegate_task" | "read_file" | "grep_search" | "glob")
  });
  ```
  When the model returns multiple `delegate_task` calls in one turn, they are spawned concurrently via `tokio::task::spawn_blocking`.
  However:
  1. [`CrashJournal`](file:///home/fredrik/marmel/wip/src/orchestrator/freeze.rs) maintains a single file (`.session_frozen.json`) holding a single `FreezeSnapshot`. Concurrent workers overwrite each other's snapshots.
  2. When worker A completes, its `clear(worker_id_A)` fails to match the snapshot (which was overwritten by worker B), leaving `.session_frozen.json` in an inconsistent state and emitting corrupted journal records.
  3. Concurrent subagents race on workspace file modifications and disk check-off in `.marmel/execution_plan.md`.
  *(Note: `manager/loop.rs:139-142` explicitly documents that `delegate_task` must be sequential per REQ-ORCH-005).*
* **Remediation:**
  Remove `"delegate_task"` from `all_parallel` in [`ui/session.rs:414`](file:///home/fredrik/marmel/wip/src/ui/session.rs#L414) so that subagent delegations are executed strictly sequentially.

---

### 1.4 Token Metric Double-Counting in LLM Client
* **Location:** [`src/llm/client.rs:451, 467, 482`](file:///home/fredrik/marmel/wip/src/llm/client.rs#L451) and [`src/llm/client.rs:324-325, 408-409`](file:///home/fredrik/marmel/wip/src/llm/client.rs#L408-L409)
* **Description:**
  In [`consume_event`](file:///home/fredrik/marmel/wip/src/llm/client.rs#L427), `record_tokens_out(1)` is called for every SSE chunk received (for content, reasoning, and tool calls).
  At stream completion, [`count_reply_tokens`](file:///home/fredrik/marmel/wip/src/llm/client.rs#L39) encodes the full reply with BPE and calls `record_tokens_out(out_toks)` a second time:
  ```rust
  let out_toks = count_reply_tokens(&reply.content, &reply.reasoning, &reply.tool_calls);
  record_tokens_out(out_toks);
  ```
  This inflates output token metrics (`GLOBAL_TOKENS_OUT` and TUI token counters) to roughly double the actual usage.
* **Remediation:**
  Remove `record_tokens_out(1)` from chunk consumption, or track stream progress using a local chunk counter without polluting the global token counter.

---

### 1.5 JSON Steer Streaming Parser Latch Bug
* **Location:** [`src/orchestrator/steer.rs:96-102`](file:///home/fredrik/marmel/wip/src/orchestrator/steer.rs#L96-L102)
* **Description:**
  [`StreamingResponseExtractor`](file:///home/fredrik/marmel/wip/src/orchestrator/steer.rs#L70) looks for `"response"` in the incoming JSON:
  ```rust
  if let Some(colon_pos) = rest.find(':') {
      let after_colon = &rest[colon_pos + 1..];
      if let Some(quote_pos) = after_colon.find('"') {
          self.in_response_field = true;
  ```
  If the LLM outputs `"response": null` (common for `QueueAndContinue` or `AbortImmediately`), `after_colon.find('"')` ignores `null,` and latches onto the opening quote of the *next* JSON key (e.g. `"tier"` or `"subtasks"`). It sets `in_response_field = true` and streams subsequent JSON syntax to the user as direct prose.
* **Remediation:**
  Verify that `after_colon.trim_start().starts_with('"')`. If it starts with `null` or any character other than `"`, `in_response_field` must remain false.

---

### 1.6 TUI Visual Corruption by MCP Stdio Subprocesses
* **Location:** [`src/mcp/client.rs:74`](file:///home/fredrik/marmel/wip/src/mcp/client.rs#L74)
* **Description:**
  When spawning stdio-based MCP servers:
  ```rust
  cmd.stderr(Stdio::inherit());
  ```
  In TUI mode (under `crossterm` raw mode and alternate screen), any logging or error output written to `stderr` by an MCP subprocess writes directly over the Ratatui render buffer, corrupting the terminal UI.
* **Remediation:**
  Configure MCP child processes with `Stdio::piped()` or redirect their stderr to a dedicated log/debug file.

---

## 2. Configuration & Performance Issues

### 2.1 `--config` CLI Argument Discarded in Specialist Runner
* **Location:** [`src/agents/runner.rs:60`](file:///home/fredrik/marmel/wip/src/agents/runner.rs#L60) & [`src/orchestrator/mod.rs:604-608`](file:///home/fredrik/marmel/wip/src/orchestrator/mod.rs#L604-L608)
* **Description:**
  When running `marmel --config custom_config.toml`, `main.rs` parses the custom config. However:
  1. [`try_run_specialist_live`](file:///home/fredrik/marmel/wip/src/agents/runner.rs#L41) re-reads configuration from disk with `crate::config::load(None).ok()?`, ignoring the custom path and falling back to default locations.
  2. [`handle_delegate_task`](file:///home/fredrik/marmel/wip/src/orchestrator/mod.rs#L604) instantiates a hardcoded fallback client:
     ```rust
     ChatClient::new("http://127.0.0.1:11434/v1", "marmel-manager")
     ```
* **Remediation:**
  Persist the active `Config` in a shared global (`ArcSwap` or `OnceLock`) or pass the resolved config through the execution harness to avoid re-reading disk defaults.

---

### 2.2 Synchronous Disk I/O on Every TUI Render Frame
* **Location:** [`src/ui/tui/render.rs:149-155`](file:///home/fredrik/marmel/wip/src/ui/tui/render.rs#L149-L155)
* **Description:**
  During each render pass in [`TuiRenderer::draw`](file:///home/fredrik/marmel/wip/src/ui/tui/render.rs#L144) (triggered on every keystroke, timer tick, and SSE streaming token), the execution plan is read from disk synchronously:
  ```rust
  let content = std::fs::read_to_string(&plan_path)...
  ```
  This causes unnecessary filesystem operations (hundreds of reads per second during fast token streaming).
* **Remediation:**
  Cache the plan content and re-read only when `std::fs::metadata(&plan_path)?.modified()` indicates changes, or update the in-memory cache reactively via tool events.

---

### 2.3 Long-Running Synchronous Tool Execution on Tokio Worker Threads
* **Location:** [`src/agents/runner.rs:533`](file:///home/fredrik/marmel/wip/src/agents/runner.rs#L533) & [`src/agents/validation.rs:291`](file:///home/fredrik/marmel/wip/src/agents/validation.rs#L291)
* **Description:**
  While [`ui/session.rs`](file:///home/fredrik/marmel/wip/src/ui/session.rs) wraps tool dispatches in `tokio::task::spawn_blocking`, both the specialist runner and validator loops execute [`dispatch_for_with_engine`](file:///home/fredrik/marmel/wip/src/harness/mod.rs#L356) synchronously on the async worker thread. Long commands (such as compilation or test suites with a 60–300s timeout) block the Tokio worker thread from making progress on other tasks.
* **Remediation:**
  Wrap synchronous dispatches in `tokio::task::block_in_place` or `tokio::task::spawn_blocking`.

---

## 3. Architecture & Refactoring Recommendations

1. **Deduplicate Turn Loop State Machines (`AgentLoop` vs `run_session`):**
   - [`AgentLoop`](file:///home/fredrik/marmel/wip/src/manager/loop.rs#L153) is primarily exercised in unit tests, while [`run_session`](file:///home/fredrik/marmel/wip/src/ui/session.rs#L18) implements the live runtime.
   - These implementations have drifted in tool classification, mid-flight steering, and abort handling.
   - Refactor `run_session` to drive or share the core state machine from `AgentLoop`.

2. **Unify `/reset` Command Handling:**
   - [`ui/helpers.rs:177`](file:///home/fredrik/marmel/wip/src/ui/helpers.rs#L177) properly clears the plan, deletes `.session_transcript.json`, and updates `ContextEngine`.
   - [`ui/bridge.rs:184`](file:///home/fredrik/marmel/wip/src/ui/bridge.rs#L184) contains an incomplete duplicate that only calls `plan.clear()`.
   - Replace the ad-hoc reset in `bridge.rs` with `handle_reset_command`.

3. **Update Outdated Docstrings:**
   - In [`src/harness/fs.rs:5`](file:///home/fredrik/marmel/wip/src/harness/fs.rs#L5), the module header describes `read_file` as line-paginated with `{line_num} | {content}`.
   - The actual implementation and schema in [`types.rs:191-209`](file:///home/fredrik/marmel/wip/src/types.rs#L191-L209) use character offsets and limits (`offset`, `limit`).
   - Update `fs.rs` documentation to reflect the character-based pagination model.
