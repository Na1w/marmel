# Quality Assessment Report: Marmel Rust Codebase

## 1. Test Inventory
**Total Test Files Analyzed:** 22
- **Integration Tests (10):** `tests/test_agent.rs`, `tests/test_context.rs`, `tests/test_harness.rs`, `tests/test_llm.rs`, `tests/test_monitor.rs`, `tests/test_orchestrator.rs`, `tests/test_role_gating.rs`, `tests/test_ui_session.rs`, `tests/test_validation_loop.rs`, `tests/common/mod.rs`.
- **Unit Test Modules (12):** `src/harness/fs_tests.rs`, `src/harness/monitor_tests.rs`, `src/harness/pty_tests.rs`, `src/llm/client_tests.rs`, `src/llm/stream_tests.rs`, `src/llm/thinking_tests.rs`, `src/manager/context_tests.rs`, `src/manager/loop_tests.rs`, `src/manager/phase_tests.rs`, `src/ui/session_tests.rs`, `src/ui/tui/tests.rs`, `src/orchestrator/tests.rs`.

**Subsystem Coverage:**
- **Well-Tested:** LLM Streaming/Thinking, Context Engine (compaction/rebirth), Orchestrator Delegation (recursion depth), Role Gating, and Filesystem Tools (replace/read).
- **Under-Tested:** Sandbox Enforcement (no negative tests for path escapes), TUI Rendering (mostly helper tests), MCP Transport.

## 2. Test Strategy Assessment
- **Mocks/Stubs:** Extensive use of `wiremock` for LLM API simulation and `tempfile` for filesystem isolation.
- **Determinism:** High for unit tests. Integration tests use deterministic "canned" deliverables for subagents to avoid live LLM dependency.
- **Flakiness Risks:** Identified usage of `std::thread::sleep` (e.g., `tests/test_harness.rs`) and `Duration::from_millis` in PTY tests, which may cause instability in CI environments.
- **Fixtures:** Consistent use of temporary directories and scripted SSE payloads.

## 3. Coverage Gap Analysis
- **Sandbox Enforcement:** `src/harness/sandbox.rs` implements Landlock, but there are no tests verifying that a process is actually blocked from accessing `/etc/passwd` or other forbidden paths.
- **TUI Rendering:** `src/ui/tui/tests.rs` tests string helpers, but there is no integration test verifying the full render loop under various terminal sizes.
- **Preemption/Freeze Recovery:** While logic exists in `src/orchestrator/mod.rs`, integration coverage is lower compared to the standard delegation flow.

## 4. Code Quality Signals
- **Panic Density:** `unwrap()` and `expect()` are frequent in `tests/` (acceptable) and occasionally in production code (e.g., `src/harness/fs.rs` in `resolve_safe_path`), which could lead to crashes on unexpected FS states.
- **Unsafe Usage:** Confined to `src/harness/pty_tests.rs` for `libc::kill` calls to manage process groups.
- **Error Handling:** Generally consistent use of `anyhow::Result`.
- **Hygiene:** No significant `TODO` or `FIXME` markers found in the audited scope.

## 5. Risk Matrix
| Risk ID | Severity | Location | Description |
|---|---|---|---|
| R1 | High | `src/harness/sandbox.rs:25` | Landlock fallback: On non-Linux or unsupported kernels, the sandbox fails open (returns `Ok(())`), leaving the system unprotected. |
| R2 | Medium | `tests/test_harness.rs` | Flakiness: Use of `std::thread::sleep` for timing-dependent assertions. |
| R3 | Medium | `src/harness/fs.rs` | Potential panic: `unwrap()` in `resolve_safe_path` on canonicalization failures. |
| R4 | Low | `src/ui/tui/mod.rs` | Complexity: High state-machine complexity in TUI rendering without full integration tests. |

## 6. Concrete Recommendations
1. **(P0) Sandbox Negative Tests:** Add tests that attempt to read forbidden files from within the sandbox to verify enforcement.
2. **(P1) Async Timers:** Replace `std::thread::sleep` with `tokio::time::sleep` or polling mechanisms in integration tests.
3. **(P2) Robust Path Resolution:** Replace `unwrap()` with proper error handling in `src/harness/fs.rs`'s `resolve_safe_path`.
4. **(P3) TUI Integration:** Implement a headless terminal test to verify TUI rendering logic.