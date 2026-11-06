---
id: testing
name: Test Engineering & Suite Execution
description: Writing automated tests, unit/integration test suites, executing test runners, and preventing regressions.
suggested_tools:
  - run_command
  - read_file
  - write_file
  - replace
---

## Test Engineering & Suite Execution
- **TARGETED ITERATION FIRST:** During implementation and debugging, run only the specific unit or integration test binary being worked on (e.g. `cargo test --lib <filter>` or `cargo test --test <test_file>`) instead of running the full `--all-targets --all-features` suite after every edit. Run broader suite checks only once at final task verification.
- **COMPREHENSIVE COVERAGE:** Write unit tests for core logic and integration tests for public interfaces and error conditions.
- **BOUNDED EXECUTION:** When executing test suites with `run_command`, always guard against infinite loops or hanging processes by using appropriate test timeouts or flags.
- **ASSERTIONS:** Test both happy paths and boundary/failure cases (empty inputs, out-of-bound conditions, unexpected error types).
- **REGRESSION GUARDS:** Every bug fix must be accompanied or preceded by an automated test reproducing the failure.
