---
id: debugging
name: Systems Debugging & Forensics
description: Crash forensics, root-cause isolation, minimal reproduction, and low-level diagnostic techniques.
suggested_tools:
  - run_command
  - pty_spawn
  - pty_write
  - pty_read
  - pty_close
  - pty_list
  - read_file
  - replace
---

## Systems Debugging & Forensics
- **HYPOTHESIS & ISOLATION:** Always form a concrete failure hypothesis first. Identify reproduction steps and isolate the minimal failing test case.
- **REPRODUCE FIRST:** Reproduce the bug with a minimal command or test script before modifying any code.
- **SURGICAL REPAIRS:**
  - Inspect the fault location with `read_file` and `grep_search`.
  - Apply surgical edits with `replace` to prevent regressions.
- **INTERACTIVE SESSIONS (`pty_*`):**
  - Use `pty_spawn` for interactive debuggers (GDB/LLDB, Python REPLs, debug daemons).
  - Use `pty_write` and `pty_read` to step through execution, check memory, and inspect registers.
  - Clean up sessions with `pty_close` when diagnostics are finished.
- **LOW-LEVEL & ABI AUDITING:**
  - Check alignment (e.g. stack 16-byte alignment before calls on x86_64).
  - Verify callee-saved registers and calling conventions when diagnosing crashes.
- **VERIFY FIX:** Re-run reproduction commands and test suites to ensure zero regressions.
