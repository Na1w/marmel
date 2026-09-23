# Marmennill — Baseline Before Refactor

- **Timestamp (UTC):** 2026-09-22T15:12:40Z
- **Git commit:** `a0b0b1ee0d42215806e888618529c5faa434072c`
- **Workspace:** `/home/fredrik/marmel/wip`
- **Crate:** marmennill v0.9.0 (binary `marmel`), Rust 2024 edition

Raw logs: `docs/refactor_audit/tmp/build.log`, `clippy.log`, `clippy_dw.log`, `test.log`.

## 1. Build (`cargo build --all-targets`)

**Status: SUCCESS** (exit 0)

```
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.28s
```

## 2. Clippy Lint Baseline (`cargo clippy --all-targets`)

**Status: CLEAN** — 0 warnings, 0 errors.

- `cargo clippy --all-targets` → exit 0, no warnings emitted.
- `cargo clippy --all-targets -- -D warnings` → exit 0 (passes even with `-D warnings`).

| File | Line | Message |
|------|------|---------|
| —    | —    | (none)  |

> This is the lint baseline: any NEW clippy warning introduced by the refactor is a regression.

## 3. Test Results (`cargo test`)

**Overall: 1 PRE-EXISTING FAILURE** in `tests/test_orchestrator.rs` (exit 101).

| Test target | Tests run | Passed | Failed | Ignored |
|---|---:|---:|---:|---:|
| `unittests src/lib.rs` (marmennill) | 334 | 334 | 0 | 0 |
| `unittests src/main.rs` (marmel) | 4 | 4 | 0 | 0 |
| `tests/test_agent.rs` | 2 | 2 | 0 | 0 |
| `tests/test_context.rs` | 1 | 1 | 0 | 0 |
| `tests/test_dynamic_prompts.rs` | 4 | 4 | 0 | 0 |
| `tests/test_harness.rs` | 2 | 2 | 0 | 0 |
| `tests/test_llm.rs` | 3 | 3 | 0 | 0 |
| `tests/test_monitor.rs` | 2 | 2 | 0 | 0 |
| `tests/test_orchestrator.rs` | 11 | 10 | **1** | 0 |
| **Total** | **371** | **370** | **1** | **0** |

### Pre-existing failure (must not be regressed, ideally fixed by refactor)

- **Test:** `test_handle_delegate_task_rejection_anchoring` (`tests/test_orchestrator.rs`)
- **Panic:** `tests/test_orchestrator.rs:333:5` — `assertion failed: res.is_error`
- Note: execution stopped after this target, so any test targets scheduled after `test_orchestrator` did not run in this invocation.

## 4. LOC per top-level src/ module (`find src -name '*.rs' | xargs wc -l`)

**Total: 34,046 lines** across all `src/*.rs` files.

| Module | LOC |
|---|---:|
| `src/ui/` | 10,410 |
| `src/harness/` | 5,111 |
| `src/orchestrator/` | 4,709 |
| `src/agents/` | 4,107 |
| `src/manager/` | 4,024 |
| `src/llm/` | 2,407 |
| `src/mcp/` | 1,125 |
| top-level files (`lib.rs`, `main.rs`, `config.rs`, `types.rs`, `prompts.rs`, `debug_log.rs`, `tool_names.rs`) | 2,153 |

### Largest individual files (top 15)

| File | LOC |
|---|---:|
| `src/ui/tui/tests.rs` | 2,240 |
| `src/ui/bridge.rs` | 1,467 |
| `src/harness/monitor.rs` | 1,261 |
| `src/ui/session.rs` | 1,227 |
| `src/harness/mod.rs` | 1,188 |
| `src/ui/tui/render.rs` | 1,098 |
| `src/ui/tui/mod.rs` | 1,083 |
| `src/agents/validation.rs` | 911 |
| `src/llm/stream.rs` | 878 |
| `src/orchestrator/mod.rs` | 855 |
| `src/agents/runner/execution.rs` | 797 |
| `src/manager/loop.rs` | 784 |
| `src/manager/context_tests.rs` | 751 |
| `src/ui/tui/formatting.rs` | 739 |
| `src/agents/prompt_builder.rs` | 736 |
