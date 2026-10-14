# Cross-platform gating feasibility report (Linux box + CI)

**Scope:** read-only feasibility research for crate `marmennill` (workspace root `/home/fredrik/marmel/wip`).
No source, manifest or workflow file was modified while producing this report. Every command below was
actually executed on this box; exits, timings and diagnostics are verbatim captures.

**Measured environment**

| Item | Value |
|---|---|
| Host triple | `x86_64-unknown-linux-gnu` (`rustup show`, Default host) |
| `cargo --version` | `cargo 1.98.0 (797e8a9bc 2026-08-05)` |
| `rustc --version` | `rustc 1.98.0 (88d9e12ae 2026-08-18)` |
| Toolchain | `stable-x86_64-unknown-linux-gnu` (active, default) |
| Package | `marmennill` 1.0.0, `edition = "2024"`, `rust-version = "1.98"` (`Cargo.toml:4-5`) |
| `[lints]` table in `Cargo.toml` | **absent** — `grep -n "\[lints" Cargo.toml` → no match |
| `[features]` table in `Cargo.toml` | absent (`grep -c "^\[features\]"` → 0) — so `--all-features` is a no-op today |
| `rustup target list --installed` | `aarch64-pc-windows-gnullvm`, `aarch64-pc-windows-msvc`, `arm64ec-pc-windows-msvc`, `thumbv6m-none-eabi`, `thumbv7m-none-eabi`, `thumbv8m.main-none-eabihf`, `wasm32-unknown-unknown`, `x86_64-pc-windows-gnu`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu` — **no `*-apple-darwin`** |
| C cross toolchains present | `/usr/bin/x86_64-w64-mingw32-gcc`, `/usr/bin/i686-w64-mingw32-gcc`, `/usr/bin/clang`; **no** MSVC SDK, **no** osxcross/Apple SDK |
| `.cargo/config.toml` | does not exist |

---

## Current CI (verbatim job/target/command table)

Source of truth: `.github/workflows/ci.yml` (106 lines). Trigger: `push`/`pull_request` on `main`/`master`
+ `workflow_dispatch`; `concurrency` cancels in-progress runs (`.github/workflows/ci.yml:3-12`). Global env:
`CARGO_TERM_COLOR=always`, `RUST_BACKTRACE=1` (`:14-16`).

| Job (`name`) | `runs-on` | matrix target | Steps / exact commands | `fmt` | clippy `-D warnings` | `test` | line refs |
|---|---|---|---|---|---|---|---|
| `lint` — *Lint & Format* | `ubuntu-latest` | none (host = `x86_64-unknown-linux-gnu`) | `cargo fmt --all -- --check`<br>`cargo clippy --all-targets --all-features -- -D warnings` | ✅ | ✅ **only here, host triple only** | ❌ | `:19-38` |
| `test` — *Test (ubuntu-latest)* | `ubuntu-latest` | `x86_64-unknown-linux-gnu` | `cargo build --all-targets --all-features --verbose`<br>`cargo test --all-targets --all-features --verbose`<br>`cargo test --doc` | ❌ | ❌ **no** | ✅ | `:40-75` |
| `test` — *Test (macOS)* (`os: macos-latest`) | `macos-latest` | `aarch64-apple-darwin` | same three commands as above | ❌ | ❌ **no** | ✅ | `:49-50`, `:68-75` |
| `test` — *Test (Windows)* (`os: windows-latest`) | `windows-latest` | `x86_64-pc-windows-msvc` | same three commands as above | ❌ | ❌ **no** | ✅ | `:51-52`, `:68-75` |
| `release-build` — *Release Build (ubuntu/mac/windows)* | `ubuntu-latest`, `macos-latest`, `windows-latest` | `x86_64-unknown-linux-gnu`, `aarch64-apple-darwin`, `x86_64-pc-windows-msvc` | `cargo build --release --verbose` | ❌ | ❌ **no** | ❌ | `:77-106` |

Matrix mechanics worth noting:

* `test` and `release-build` use `strategy.matrix.include` with `fail-fast: false`, and `test` `needs: lint`,
  `release-build` `needs: test`.
* `matrix.target` is **only** consumed by `dtolnay/rust-toolchain@stable with targets: ${{ matrix.target }}`
  (`:60-61`, `:97-98`) and as a `rust-cache` key. **No build/test/build-release command passes `--target`.**
  On the mac/windows runners the host triple happens to equal the matrix target, so this is currently benign,
  but nothing in CI enforces that equality.
* The doc-test step `cargo test --doc` (`:74-75`) is the only test command **without** `--all-features`.
* Note also: `cargo test --doc` runs doc-tests only; `--all-targets` never runs them. Today the crate has
  0 doc-tests (`docs/baseline_test_run.md:127`), so this gap is currently vacuous.

**Answer to the explicit question:** `-D warnings` appears in exactly one place in the whole workflow —
`.github/workflows/ci.yml:38`, the `lint` job on `ubuntu-latest`, via clippy, for the **host triple only**.
`Test (macOS)` (`macos-latest` / `aarch64-apple-darwin`), `Test (Windows)` (`windows-latest` /
`x86_64-pc-windows-msvc`), `Test (ubuntu-latest)` and all three `Release Build` entries run **no** clippy,
**no** fmt and **no** `-D warnings`. That is the gap: warnings that exist only on the macOS/Windows cfg
branches compile successfully and are silently ignored by CI.

---

## What CI does NOT check

1. **Target-specific rustc warnings / dead code.** Proven on this box: `cargo check --all-targets --target
   x86_64-pc-windows-gnu` emits **17 warnings** that do **not** exist for the host target, and none of the
   non-`lint` CI jobs would turn them into failures (`cargo build`/`cargo test` tolerate warnings):

   | # | Diagnostic | Site |
   |---|---|---|
   | 1 | `unused import: crate::harness::pty::SANDBOX_EXEC_ARG` | `src/harness/sandbox.rs:499:9` (import is `#[cfg(target_os = "linux")]`, module `#[cfg(test)] mod tests` at `:496`) |
   | 2 | `unused imports: JsonRpcNotification and JsonRpcRequest` | `src/mcp/client.rs:632:32` |
   | 3 | `unused import: JsonRpcTransport` | `src/mcp/client.rs:632:69` |
   | 4 | `struct CaptureWriter is never constructed` | `src/harness/sandbox.rs:510:12` |
   | 5 | `struct CaptureSink is never constructed` | `src/harness/sandbox.rs:512:12` |
   | 6 | `function capture_logs is never used` | `src/harness/sandbox.rs:535:8` |
   | 7 | `constant MCP_MARKER is never used` | `src/harness/tool_resolution_tests.rs:24:7` |
   | 8 | `static REGISTRY_LOCK is never used` | `src/harness/tool_resolution_tests.rs:28:8` |
   | 9 | `function registry_guard is never used` | `src/harness/tool_resolution_tests.rs:30:4` |
   | 10 | `function clear_global_mcp_registry is never used` | `src/harness/tool_resolution_tests.rs:36:4` |
   | 11 | `function multi_thread_runtime is never used` | `src/harness/tool_resolution_tests.rs:42:4` |
   | 12 | `struct ScriptedServer is never constructed` | `src/harness/tool_resolution_tests.rs:52:8` |
   | 13 | `function scripted_server is never used` | `src/harness/tool_resolution_tests.rs:57:4` |
   | 14 | `function boot_manager is never used` | `src/harness/tool_resolution_tests.rs:106:10` |
   | 15 | `struct ScriptedMcp is never constructed` | `src/llm/client_tests.rs:636:8` |
   | 16 | `function scripted_mcp is never used` | `src/llm/client_tests.rs:641:4` |
   | 17 | `static MCP_REGISTRY_LOCK is never used` | `src/llm/client_tests.rs:678:8` |

   Mechanism (verified, not inferred): the helpers live in `#[cfg(test)]` modules
   (`src/harness/mod.rs:25-26`, `src/harness/mod.rs:1126-1127`), but the *tests that use them* are gated
   `#[cfg(unix)]` (`src/harness/tool_resolution_tests.rs:141,190,269,437,502`;
   `src/llm/client_tests.rs:684`; `src/mcp/client.rs:637`-region) or
   `#[cfg(target_os = "linux")]` (`src/harness/sandbox.rs:499`). Compile for Windows ⇒ the callers vanish ⇒
   the helpers become dead code. Total cfg surface involved (`grep` over `src/`): `cfg(unix)` ×36,
   `cfg(target_os = "linux")` ×21, `cfg(not(unix))` ×8, `cfg(windows)` ×3.
2. **A real lib-level lint that only fires for the Windows cfg:**
   `cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings` reports
   `error: unnecessary closure used to substitute value for Option::None` at **`src/config.rs:537:5`**
   (the `std::env::var_os("HOME")…or_else(|| { #[cfg(unix)] … })` home-dir fallback). Host clippy
   (`--all-targets --all-features -- -D warnings`) exits **0** in 4.86 s and does not see it. So today CI
   has a *non-test*, Windows-only lint that nobody checks.
3. **Nothing else target-related:** no `cargo fmt`/clippy on macOS/Windows runners, no `--target` in any
   build/test command, no release-profile lint leg, no clippy for the release profile, no cross-target
   pre-triage (a Windows-only compile error costs one full CI round-trip on a `windows-latest` runner).
4. **Not a gap but a caveat:** CI's mac/windows legs do run the PTY tests natively, so the sandbox-local
   skips listed later are *not* skips in CI.

---

## Local cross-target feasibility (commands + outcomes)

### 3.1 `cargo check --all-targets --target x86_64-pc-windows-gnu` — **works**

```
$ cargo check --all-targets --target x86_64-pc-windows-gnu
exit 0   (warm cache: "Finished dev profile … in 0.27s")
18 ^warning lines = 17 diagnostics (table above) + 1 "generated 17 warnings" summary; 0 errors
tail: "warning: `marmennill` (lib test) generated 17 warnings (run `cargo fix --lib -p marmennill --tests` …)"
```

So the warning report in the previous task is confirmed: **warnings, not errors**. Cross-compiling for
Windows-GNU needs no linker and the two C build scripts succeed because `cc-rs` auto-selects the installed
`x86_64-w64-mingw32-gcc`; evidence: `target/x86_64-pc-windows-gnu/debug/build/aws-lc-sys-*/out/*.o` and
`…/build/onig_sys-*/` both exist. Pure-Rust UI/terminal/token deps (`crossterm`, `portable-pty`,
`filedescriptor`, `tiktoken-rs`) compile unchanged for the Windows target.

Escalating to hard errors on the same target (proven):

```
$ cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings
exit 101, 21 s   →  "error: could not compile `marmennill` (lib) due to 1 previous error"
                 →  "error: could not compile `marmennill` (lib test) due to 14 previous errors"
```
Observed oddity (factual, flagged for whoever builds the gate): that clippy invocation did **not** replay the
four `src/harness/sandbox.rs` diagnostics that plain `cargo check` emitted on the same target, but did add the
`src/config.rs:537` clippy lint. ⇒ run **both** legs; they are not interchangeable.

### 3.2 `cargo check --all-targets --target x86_64-pc-windows-msvc` — **fails locally (build scripts)**

```
$ cargo check --all-targets --target x86_64-pc-windows-msvc
exit 101
error: failed to run custom build command for `onig_sys v69.9.3`
  warning: onig_sys@69.9.3: GNU compiler is not supported for this target   (repeated)
error: failed to run custom build command for `aws-lc-sys v0.44.0`
  error occurred in cc-rs: command did not execute successfully (status code exit status: 1):
  LC_ALL="C" "cc" "-O0" … "-std=c11" … "-DBORINGSSL_PREFIX=aws_lc_0_44_0" … "-c"
      "…/aws-lc/crypto/x509/x_x509.c"
```
`rust-std-x86_64-pc-windows-msvc` **is** installed, so this is purely a missing MSVC-targeting C toolchain
(`cc` resolves to host GCC; no clang-cl/MSVC headers; no osxcross). C-crate chain confirmed in `Cargo.lock`:
`reqwest → rustls → aws-lc-rs → aws-lc-sys`, and `tui-markdown → syntect → onig → onig_sys` (`Cargo.lock`
`onig`@6.5.3, `onig_sys`@69.9.3, `aws-lc-sys`@0.44.0; `ring`@… also pulled by `quinn-proto`/`rustls-webpki`).
**Conclusion: do not put `x86_64-pc-windows-msvc` in the local script; use `x86_64-pc-windows-gnu`.**

### 3.3 `rustup target add aarch64-apple-darwin` + `cargo check --target aarch64-apple-darwin` — **blocked**

```
$ rustup target add aarch64-apple-darwin            exit 1
info: downloading component rust-std
info: rolling back changes
error: component download failed for rust-std-aarch64-apple-darwin: error opening file for download:
      cleaning up cached downloads: No such file or directory (os error 2)
```
First blocking error is the **toolchain install**, not a dependency build script. Root cause established:

* Network is fine: `curl -sSI https://static.rust-lang.org/dist/channel-rust-stable.toml` → `HTTP/2 200`;
  `curl` of `…/dist/2026-08-20/rust-std-1.98.0-aarch64-apple-darwin.tar.xz` → `200`, 29 740 736 bytes.
* `rustup -v` shows the download URL is reached, then the failure is in rustup's cached-download handling.
* `~/.rustup` is **read-only in this sandbox**: `touch` probe → `READ-ONLY: /home/fredrik/.rustup`,
  `READ-ONLY: …/downloads`, `READ-ONLY: …/tmp`, `READ-ONLY: …/toolchains`.
* Not Apple-specific: `rustup target add x86_64-unknown-linux-musl` fails identically, so **no new target can
  be installed on this box at all**.

Consequence of the missing std:

```
$ cargo check --all-targets --target aarch64-apple-darwin     exit 101
error[E0463]: can't find crate for `core`
error[E0463]: can't find crate for `std`
error: could not compile `scopeguard` (lib) due to 1 previous error   (and every other dep)
```

Even if the std were installed outside this sandbox (one command in a normal shell), the **next** blocker on
Linux→macOS would be the same two C build scripts proven in §3.2 (`onig_sys`, `aws-lc-sys`), which need an
Apple-targeting clang plus the macOS SDK — absent here (no osxcross). *Inference, clearly marked:* Apple
compile parity is a **CI-only** guarantee on this machine; the script must treat `aarch64-apple-darwin` as
optional and say so loudly.

### 3.4 What `check` vs `test` per target actually buys (semantics)

* `cargo check --all-targets --target T` (and `cargo clippy … --target T`) = parse + typecheck + lint of the
  crate and **every cfg branch reachable for T**, including `#[cfg(test)]`/`#[cfg(unix)]`/`#[cfg(windows)]`
  variants, without linking and without producing runnable code. Cheap: 0.27 s warm per extra target once
  dependency artifacts exist (21 s warm for the clippy leg).
* `cargo test --target T` would additionally require **linking and executing** a Windows/macOS binary — that
  needs a real linker for the target and an emulator/runtime (`wine`, and no practical macOS emulator). Not
  feasible and not attempted here; nothing in this environment provides it.

**Therefore the local gate can guarantee:** typecheck + lint + dead-code/unused-import parity across
Linux/Windows(-gnu)/Apple-Darwin cfg branches, i.e. "it compiles and is warning-clean for every target".
**It cannot guarantee** test behaviour on macOS/Windows: PTY semantics, landlock, console/`crossterm`
terminal handling, process-group killing, path/separators at runtime, or anything that only manifests when
the binary runs. Those remain 100 % dependent on the three CI `test` legs.

---

## Pre-existing env-dependent test failures (list + handling)

**Environment cause (verified):** `stat -c '%A %a %U:%G %n' /dev/pts/ptmx` → `c--------- 0 root:root`
(mode `000`), so `openpty()` fails: `failed to openpty: Os { code: 13, kind: PermissionDenied }`. There is
**no** `#[ignore]` anywhere in the repo (`grep -rn "#\[ignore" src tests` → 0 hits), so these run everywhere.

Measured matrix (`cargo test --lib`, same binary, no source change):

| Mode | Command | Exit | Result | Time |
|---|---|---|---|---|
| default (parallel) | `cargo test --lib` | 101 | 805 passed / **10 failed** | 16.7 s |
| serial | `cargo test --lib -- --test-threads=1` | 101 | 810 passed / **5 failed** | 83.8 s |
| parallel + skip 5 names | `cargo test --lib -- --skip <5 names>` | 101 | 806 passed / **4 failed** (cascades remain) | 16.6 s |
| **serial + skip 5 names** | `cargo test --lib -- --test-threads=1 --skip <5 names>` | **0** | **810 passed / 0 failed** | 83.7 s |

The 5 **deterministic, env-dependent** failures (present in serial mode; all PTY-dependent):

1. `harness::pty::tests::test_harness_pty_process_group_kill` — `src/harness/pty_tests.rs:16` `expect("spawn sandbox")` → `openpty … PermissionDenied`
2. `harness::pty::tests::test_interactive_pty_manager_lifecycle` — `src/harness/pty_tests.rs:121` → same
3. `harness::pty::tests::test_run_command_parses_timeout_seconds_argument` — `src/harness/pty_tests.rs:102` → same
4. `harness::pty::tests::test_run_command_pty_cancelled_by_worker_token` — `assertion failed: res.is_ok()` (PTY spawn failed first)
5. `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` — `src/harness/mod.rs:1261` `assertion failed: !res.is_error`; it is PTY-dependent because the delegated `run_command` really executes `pty::run_command` (`src/harness/mod.rs:337`, `:683`, `:1083`)

The other 5 parallel-mode failures are **flaky cascades**, not separate bugs:
`harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool`
(`panicked at src/harness/pty.rs:1162: there is no reactor running, must be called from the context of a
Tokio 1.x runtime` — `PtyManager::new()` calls `tokio::spawn`), then
`LazyLock instance has previously been poisoned` in
`harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler` and
`tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool`, plus 1–2
`orchestrator::notice_tests::*` (`test_handle_reply_to_arbitrator_success_and_validation` /
`…ask_followup` / `test_steering_history_multiple_notices_resolved_out_of_order` — membership varies run to
run, also documented in `docs/baseline_test_run.md` §5.2/§5.3). Cause: these modules mutate a
**process-global MCP registry** under a shared `static REGISTRY_LOCK: std::sync::Mutex<()>`
(`src/harness/tool_resolution_tests.rs:28-34`) and `MCP_GATE` (`:36-40`); a panicking test poisons the
`LazyLock` and every parallel neighbour fails. **Skipping the PTY tests does not remove them** (row 3);
**`--test-threads=1` does** (rows 2 and 4).

Integration suites: `cargo test --test test_role_gating` → 24 passed / **1 failed**:
`test_prompt_based_tool_gating` (`tests/test_role_gating.rs:312`) — also PTY-dependent, for the same reason
as #5: it calls `dispatch_for(run_command …)` which executes the real PTY path. `fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority`
(reported in `docs/baseline_test_run.md:12,212`) did **not** reproduce in any run in this session; keep it on
a watch-list rather than skipping it.

**Exact recommendation for the gate script (validated end-to-end):**

```
$ cargo test --all-targets --no-fail-fast -- --test-threads=1 \
    --skip harness::pty::tests::test_harness_pty_process_group_kill \
    --skip harness::pty::tests::test_interactive_pty_manager_lifecycle \
    --skip harness::pty::tests::test_run_command_parses_timeout_seconds_argument \
    --skip harness::pty::tests::test_run_command_pty_cancelled_by_worker_token \
    --skip harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable \
    --skip test_prompt_based_tool_gating
exit 0 — 954 passed / 0 failed / 6 filtered, 150 s (18 test-result blocks:
  lib 810 passed/5 filtered, bin `marmel` 8, tests/test_agent 2, test_context 8, test_dynamic_prompts 4,
  test_harness 15, test_llm 5, test_midturn_notice 3, test_monitor 2, test_orchestrator 8,
  test_plan_checkoff 6, test_role_gating 24/1 filtered, test_specialist_schema_charge 4,
  test_specialist_stream 10, test_steer_history 9, test_tool_name_literals 1, test_ui_session 21,
  test_validation_loop 14)
```

Rationale: `--test-threads=1` is mandatory for determinism (it is what removes the poisoned-`LazyLock`
cascades); the 6 skips are **substring** matches, so name the exact 5 test paths instead of `harness::pty::`
(which would silently filter 30 tests, 25 of them passing and relevant). Cost: 150 s vs 78 s for the
fail-fast full run. Note the trade-off to reviewers: the skip list masks 6 real PTY executions locally, which
is acceptable **only** because CI's three `test` legs run them natively; the same is *not* true for the
`--test-threads=1` isolation weakness — that is a genuine test-isolation bug worth its own fix ticket
(`src/harness/tool_resolution_tests.rs` global-registry sharing + `tokio::spawn` inside `PtyManager::new()`).

---

## Recommended gate design

### A. `scripts/cross_platform_check.sh` — exact command list (validated line by line on this box)

```bash
#!/usr/bin/env bash
# scripts/cross_platform_check.sh — local parity gate (Linux host)
set -euo pipefail

# Keep this list identical to the CI cross-target list (see B, cross-check job),
# modulo the documented gnu/msvc alias rule.
TARGETS=( x86_64-pc-windows-gnu aarch64-apple-darwin )

# Tests that require a real PTY (openpty EACCES in this sandbox: /dev/pts/ptmx is mode 000)
SKIPS=( --skip harness::pty::tests::test_harness_pty_process_group_kill
        --skip harness::pty::tests::test_interactive_pty_manager_lifecycle
        --skip harness::pty::tests::test_run_command_parses_timeout_seconds_argument
        --skip harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
        --skip harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable
        --skip test_prompt_based_tool_gating )

# 1. Host lint/format — identical to CI job `lint` (ci.yml:35,38)          [measured: exit 0]
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings

# 2. Host tests — deterministic, env-skipped, serial                       [measured: exit 0, 150 s]
cargo test --all-targets --no-fail-fast -- --test-threads=1 "${SKIPS[@]}"
cargo test --doc --all-features          # 0 doc-tests today; mirrors ci.yml:74-75

# 3. Per-target parity: typecheck + lint of every cfg branch reachable for T [gnu: exit 101 today, 21 s]
missing=0
for T in "${TARGETS[@]}"; do
  if rustup target list --installed | grep -qx "$T"; then
    cargo clippy --all-targets --all-features --target "$T" -- -D warnings
    cargo check  --all-targets --all-features --target "$T"   # replays rustc-only diagnostics (§3.1 oddity)
  else
    echo "WARN: target $T not installed — run 'rustup target add $T' (blocked in this sandbox)" >&2
    missing=$((missing+1))
  fi
done
# Fail closed on missing std unless explicitly allowed, so "green" never means "not checked".
[ "$missing" -eq 0 ] || { echo "coverage incomplete: $missing target(s) skipped" >&2
                          exit 1; }
```

Measured cost: fmt + clippy(host) + full serial test suite + windows-gnu clippy/check ≈ **3 minutes** warm.
`cargo check --target x86_64-pc-windows-gnu` alone is 0.27 s warm; `cargo clippy … --target
x86_64-pc-windows-gnu` is 21 s warm; `aarch64-apple-darwin` is currently unmeasurable (std unavailable).

**Do not use `RUSTFLAGS="-D warnings"`** (scratch probe at `/tmp/lintprobe`, cargo 1.98):

| Variant | Outcome |
|---|---|
| plain `cargo check` | warning, exit 0 |
| `RUSTFLAGS="-D warnings" cargo check` | **also escalates inside a path dependency** (`error: function dep_unused is never used` → `could not compile dep (lib)`), exit 101, and forces a full rebuild of every unit → slow + can be red on third-party code |
| `[lints.rust] dead_code = "deny", unused = "deny"` in the *package* manifest | escalating only for the declaring package: `error: function unused_fn is never used` → `could not compile probe (lib)`, while `dep` still only warns |
| `cargo check … -- -D warnings` | rejected (`Usage: cargo check [OPTIONS]`) — trailing rustc flags are not accepted by `cargo check` |
| `cargo rustc --all-targets … -- -D warnings` | rejected (`error: extra arguments to rustc can only be passed to one target`) |

⇒ the two mechanisms that work are **`cargo clippy … --target T -- -D warnings`** (flag reaches the workspace
member only; proven above) and a **`[lints]` table in `Cargo.toml`** (does not exist today — confirmed by
`grep -n "\[lints" Cargo.toml` → no match).

### B. Recommended `Cargo.toml` addition (so per-target `-D` flags become unnecessary)

```toml
[lints.rust]
unused          = "deny"
dead_code       = "deny"
unused_imports  = "deny"

[lints.clippy]
all = { level = "deny", priority = -1 }
```
This makes plain `cargo build`/`cargo test`/`cargo check --target T` fail on target-specific warnings **in CI's
existing mac/windows legs**, with no new commands and no `RUSTFLAGS` leak into dependencies.
`rust-version = "1.98"` (`Cargo.toml:5`) supports `[lints]` (stable since 1.74).

### C. Exact CI deltas to `.github/workflows/ci.yml` — **APPLIED** (job ids and step names below are the
ones actually present in `.github/workflows/ci.yml` today; the proposal text and rationale are kept verbatim,
with the landed form recorded under each item)

Applied state of the workflow: exactly four jobs —

| job id | display name | notes |
|---|---|---|
| `lint` | "Lint & Format" | unchanged; steps "Check code formatting", "Run Clippy" |
| `test` | "Test (${{ matrix.os }})" = Test (ubuntu-latest) / Test (macos-latest) / Test (windows-latest) | `needs: lint`; toolchain step now carries `components: clippy`; new per-OS steps **"Check code formatting"** (`cargo fmt --all -- --check`) and **"Run Clippy for this target"** (`cargo clippy --all-targets --all-features -- -D warnings`) → item 1 |
| `cross-check` | "Cross-target check (windows-gnu)" | `needs: lint`, `runs-on: ubuntu-latest` → item 2 |
| `release-build` | "Release Build (${{ matrix.os }})" | untouched |

1. **Per-target clippy inside the `test` matrix — APPLIED** (this is the real fix for the gap; it lands on every OS and
   therefore covers `apple-darwin` and `windows-msvc` where cross-checking from Linux is impossible):

   Proposal sketch, kept verbatim for the record (the landed commands differ slightly — see "Landed form" below):

```yaml
      - name: Clippy for this target
        run: cargo clippy --all-targets --all-features --target ${{ matrix.target }} -- -D warnings
```
   (place it after `Set up Rust toolchain`; `components: clippy` must be added to that step's `with:`)

   **Landed form (job `test`, all three OS legs):** "Set up Rust toolchain" now has
   `with: { targets: ${{ matrix.target }}, components: clippy }`, and the job gained two steps before
   "Build all targets": **"Check code formatting"** (`cargo fmt --all -- --check`) and **"Run Clippy for this
   target"** (`cargo clippy --all-targets --all-features -- -D warnings`). Deviation from the sketch, on purpose:
   those commands carry **no** `--target` — on every matrix entry the runner host triple *is* `matrix.target`, so
   the flag would be a no-op while changing artifact layout/linker selection for the native legs. The Cargo.toml
   `[lints.rust]`/`[lints.clippy]` deny tables (§B) escalate the same commands; the explicit `-D warnings` is
   kept as defence in depth.
2. **Cheap pre-triage job for the Windows cfg class, APPLIED** — on a Linux runner, so a Windows-only lint does not cost
   a mac/windows round-trip:

   Proposal sketch, kept verbatim for the record (the landed job has no matrix — see "Landed form" below):

```yaml
  cross-check:
    name: Cross-target check (${{ matrix.target }})
    needs: lint
    runs-on: ubuntu-latest
    strategy:
      fail-fast: false
      matrix:
        target: [ x86_64-pc-windows-gnu ]      # see reconciliation rule below
    steps:
      - uses: actions/checkout@v4
      - name: Install mingw-w64 (needed by onig_sys / aws-lc-sys build scripts)
        run: sudo apt-get update && sudo apt-get install -y gcc-mingw-w64-x86-64
      - uses: dtolnay/rust-toolchain@stable
        with:
          targets: ${{ matrix.target }}
          components: clippy
      - uses: Swatinem/rust-cache@v2
        with: { key: cross-${{ matrix.target }} }
      - run: cargo clippy --all-targets --all-features --target ${{ matrix.target }} -- -D warnings
      - run: cargo check  --all-targets --all-features --target ${{ matrix.target }}
```
   (`mingw-w64` is exactly why §3.1 works and §3.2 fails; the Ubuntu runner image has apt and the package.)

   **Landed form (job `cross-check` = "Cross-target check (windows-gnu)", `needs: lint`, `runs-on:
   ubuntu-latest`, no matrix):** steps "Checkout repository" → **"Resolve cross target from shared gate list"**
   (`id: gate`; an `awk` parser over `config/gate_targets.txt` that ignores `#` comments/blank lines, requires
   exactly two fields, hard-errors on an unknown scope, filters to scope `local|both` **and** the `*-windows-gnu`
   ABI, and fails closed unless that yields exactly one triple) → **"Install mingw-w64 C toolchain (needed by
   onig_sys / aws-lc-sys build scripts)"** (`sudo apt-get update && sudo apt-get install -y gcc-mingw-w64-x86-64`)
   → "Set up Rust toolchain" (`targets: ${{ steps.gate.outputs.target }}`, `components: clippy`) → "Rust Cache"
   (`key: cross-${{ steps.gate.outputs.target }}`) → **"Run Clippy for the cross target"**
   (`cargo clippy --all-targets --all-features --target ${{ steps.gate.outputs.target }} -- -D warnings`) →
   **"Check all targets for the cross target"** (`cargo check --all-targets --all-features --target
   ${{ steps.gate.outputs.target }}`). Deviation from the sketch, and an improvement: the triple is **resolved from
   the single shared list** `config/gate_targets.txt` (§D) instead of a hard-coded matrix, so the local gate script
   and this job can never drift apart. BOTH legs (clippy **and** check) are present because §3.1 measured that they
   are not interchangeable. The job is deliberately **not** a `needs:` of `test` or `release-build` (residual risk
   7), and `lint` gates it exactly like `test`.
3. **Matrix hygiene — NOT APPLIED, deliberately rejected:** add `--target ${{ matrix.target }}` to `Build all targets` and `Run unit & integration
   tests` (`ci.yml:69,72`) and add `--all-features` to the doc-test step (`ci.yml:75`) so the three commands
   provably use the same target/feature set as the toolchain step.

   **Why it was rejected:** `--target ${{ matrix.target }}` on the native legs is a no-op for target selection
   (runner host triple == `matrix.target`) but does change artifact layout and linker selection, and it would make
   the native legs behave like cross legs; the applied job keeps those steps as `cargo build --all-targets
   --all-features --verbose`, `cargo test --all-targets --all-features --verbose` and `cargo test --doc`, with
   `matrix.target` consumed by the toolchain step (`targets:`) and the cache key (`key: ${{ matrix.target }}`).
   The same reasoning is on the record in a comment in `ci.yml` (above the per-OS format/clippy steps): "No
   `--target` here on purpose: on every matrix entry the runner host triple IS matrix.target, so the flag would be
   a no-op while changing artifact layout/linker selection for the native legs."
4. **Do NOT add `aarch64-apple-darwin` or `x86_64-pc-windows-msvc` to a Linux cross-check leg — RESPECTED** — both need C
   build scripts (`onig_sys`, `aws-lc-sys`) for toolchains the Linux runner does not have (proven in §3.2/§3.3).

   **How it is now enforced mechanically:** the `gate` step of job `cross-check` filters `config/gate_targets.txt`
   to scope `local|both` **and** the `*-windows-gnu` ABI and hard-fails unless that filter yields exactly one
   triple, so a non-gnu ABI can never be picked up for a Linux leg and the cross leg can never be silently widened.

### D. Script vs CI target-list reconciliation (explicit)

* The **lists cannot be literally identical**, and the difference is a toolchain fact, not a style choice:
  Linux→`x86_64-pc-windows-msvc` cross-compilation fails locally *and* on `ubuntu-latest` because
  `onig_sys`/`aws-lc-sys` need an MSVC-targeting C toolchain; Linux→`x86_64-pc-windows-gnu` works with
  `gcc-mingw-w64` (installed here, installable in CI); Linux→`aarch64-apple-darwin` needs an Apple SDK that
  exists nowhere in this setup.
* **Rule to adopt:** the gate is defined over **cfg classes**, not ABIs.
  `windows` cfg class = `x86_64-pc-windows-gnu` locally/in-CI-cross-check **and** `x86_64-pc-windows-msvc`
  natively on `windows-latest`; `apple-darwin` class = native `macos-latest` only; `linux` class = native
  `ubuntu-latest` + host. Both the script and CI must cover the same three classes, and the script must print
  which class it actually covered (and exit non-zero when a std is missing, per A) so silent degradation is
  impossible.
* **Concrete way to keep them from drifting:** keep one shared list in the repo (e.g.
  `config/gate_targets.txt` containing `x86_64-unknown-linux-gnu`, `x86_64-pc-windows-gnu`,
  `aarch64-apple-darwin`, `x86_64-pc-windows-msvc`, each annotated `local|ci|both`) and have both
  `scripts/cross_platform_check.sh` and a CI step read it, so adding a target is a single edit.
* **Also add to CI (cheap, high value):** `[lints]` per §B, which retro-fits `-D warnings` onto the existing
  mac/windows legs without touching their commands.
* **Precondition before flipping the gate red:** fix the 17 target-specific warnings in §"What CI does NOT
  check" and the `src/config.rs:537` clippy lint, or the first run of the new gate/clippy-on-target is red.
  (The typical fix is scoping the helper items with the same `#[cfg(unix)]`/`#[cfg(target_os = "linux")]` as
  their callers, or `#[cfg_attr(not(unix), allow(dead_code))]`.)

---

## Residual risk

1. **No behavioural parity locally.** Tests cannot be executed for Windows/macOS (no linker+emulator; not
   attempted). PTY/`portable-pty`, `crossterm` raw-mode/console, landlock (`cfg(target_os = "linux")` ×21),
   process-group reaping, path/separator and file-locking semantics are verified **only** by CI's three
   native `test` legs.
2. **`cargo check`/`clippy` do not link.** Link-time errors for a target (missing symbol, wrong import lib,
   `windows-sys`/`libc` symbol mismatches) are not caught by the local leg. A local
   `cargo build --target x86_64-pc-windows-gnu` might catch more (mingw gcc is present) — **not tested in this
   session**; if added to the script, budget a full link + expect mingw-version sensitivity.
3. **MSVC-only differences remain CI-only**: `#[cfg(target_env = "msvc")]` branches, MSVC C-ABI for
   `aws-lc-sys`/`onig_sys`, and MSVC-specific diagnostics. `windows-gnu` parity ≠ `windows-msvc` parity; the
   gnu leg is a *cfg-class* proxy that shortens feedback, never a replacement for `Test (Windows)`.
4. **Apple-Darwin is currently unchecked locally, entirely.** `rustup target add` is impossible in this
   sandbox (`~/.rustup` read-only: `READ-ONLY: /home/fredrik/.rustup{,/downloads,/tmp,/toolchains}`; the same
   failure occurs for `x86_64-unknown-linux-musl`), and even with std an Apple SDK would be required for
   `onig_sys`/`aws-lc-sys`. Until the std is installed outside the sandbox, the script's `apple-darwin` leg
   only prints a WARN — hence the fail-closed rule in A, and the CI mac leg is load-bearing.
5. **Six skipped tests locally** (5 lib + `test_prompt_based_tool_gating`) are exactly the PTY-dependent ones;
   a regression confined to PTY code is invisible to the local gate. `fix_loop_dispatcher_refuses_a_verdict_tool_without_verdict_authority`
   is on a watch-list (flaky in combined runs, never reproduced here).
6. **Test isolation bug is masked, not fixed**, by `--test-threads=1`: parallel-mode poisoning of the
   process-global MCP registry / `LazyLock` (`src/harness/tool_resolution_tests.rs:28-40`) plus
   `tokio::spawn` inside `PtyManager::new()` (`src/harness/pty.rs:1162`) will keep producing run-to-run
   failure-set drift in any parallel invocation (human `cargo test`, or `docs/t007_oneshot_verify.sh`-style
   ad-hoc runs). CI runs tests in parallel (its `test` step has no `--test-threads=1`), so the local gate and
   CI use **different** test scheduling — a source of "green locally, red in CI" for isolation-sensitive
   tests. Align by adding `-- --test-threads=1` to CI's test step, or fix the isolation.
7. **Toolchain-image drift in CI:** the new `cross-check` job depends on `apt-get install gcc-mingw-w64-x86-64`
   on `ubuntu-latest`; if the image or the `aws-lc-sys`/`onig_sys` build scripts change, that leg can go red for
   environmental reasons while the native legs stay green. Keep it non-blocking for `release-build`
   (`needs: lint` only, as proposed).
8. **`--all-features` is currently vacuous** (no `[features]` in `Cargo.toml`); the moment features are added,
   the doc-test step (`ci.yml:75`) and any command missing `--all-features` will silently stop matching the
   clippy leg.
9. **`clippy` vs `check` diagnostic replay differs in practice** (the four `src/harness/sandbox.rs`
   diagnostics appeared under `cargo check` on the windows target but not under the `-D warnings` clippy run);
   keep both legs until someone explains it, otherwise a `check`-only warning class can slip through the
   clippy leg.
