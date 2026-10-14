# t-014 — Independent cross-target verdict for the platform gate

Scope: verification only. No source, script, config or CI file was modified.
Host: `x86_64-unknown-linux-gnu`, rustc `1.98.0 (88d9e12ae 2026-08-18)`, cargo `1.98.0 (797e8a9bc 2026-08-05)`,
`x86_64-w64-mingw32-gcc (GCC) 13-win32` present, `~/.rustup` **read-only** (`touch ~/.rustup/x` → `Permission denied`).
Artifacts of every run: `/tmp/gatever/run1_full.log`, `run1_full_exitcode.log`, `run2_noflag.log`,
`run3_onlyhost.log`, `run4_negflag.log`, `c4_check.log` (forced-dirty `-v`), `c4a_check.log`, `c4b_clippy.log`,
`c4c_check_v.log`, `par1.log`, `par2.log`, `par3.log`, `envrace_{1..5}.log`.
Content integrity of the audited tree was pinned before and after the audit:
`md5sum` of all 120 `src/**/*.rs` + `tests/**/*.rs` files is byte-identical pre/post (`tree_before.md5` == `tree_final.md5`)
and `git status --porcelain` is unchanged (18 entries, same set). Only mtimes and `target/` (gitignored) changed.

## Verdict: PASS

Contract items (a), (b), (c), (d) are all satisfied — evidence below. No gate/CI mismatch invalidates the verdict; nine
advisory (non-blocking) findings — including one documentation-accuracy defect in `src/config.rs` and one live
parallel-test flakiness risk — are recorded under `## Recommendations` (R1–R9).

## Acceptance contract

Verbatim from the tasking:

> PASS requires: (a) `./scripts/cross_platform_check.sh --allow-missing-target aarch64-apple-darwin` exits 0 with
> every executed leg green, (b) the `x86_64-pc-windows-gnu` `check` AND `clippy -D warnings` legs are green,
> (c) the host legs (fmt, clippy -D warnings, `cargo test --all-targets --all-features -- --test-threads=1` + 6 skips,
> doc-test) are green, and (d) every coverage gap that cannot be exercised on this box is NAMED in the output and
> mapped to the CI job that owns it.

## Checks

| # | Command | Expected | Observed | Exit |
|---|---|---|---|---|
| 1 | `./scripts/cross_platform_check.sh --allow-missing-target aarch64-apple-darwin` | exit 0, every executed leg green | `legs executed : 6 (PASS 6 / FAIL 0)   skipped-by-design rows: 3`; `VERDICT: PASS — 6 leg(s) green, 3 coverage gap row(s) explicitly accounted for above.` Table: `host:fmt PASS 1s`, `host:clippy PASS 0s`, `host:test PASS 118s`, `host:doc-test PASS 1s`, `cross:* x86_64-unknown-linux-gnu HOST-LEG (already covered)`, `cross:check x86_64-pc-windows-gnu PASS 1s`, `cross:clippy x86_64-pc-windows-gnu PASS 0s`, `cross:check+clippy aarch64-apple-darwin SKIPPED (std missing)`, `cross:* x86_64-pc-windows-msvc NOT-CHECKED (scope=ci)` | **0** |
| 1b | same, re-run for an explicit exit-code capture | idempotent | identical table, `EXIT_CODE=0` | **0** |
| 2 | `./scripts/cross_platform_check.sh` (no flag) | non-zero, loud fail-closed pre-audit | `*** GATE WILL FAIL CLOSED (not "green"): 1 target(s) listed local|both in …/config/gate_targets.txt` / `*** have NO rust-std installed, so their cfg class is NOT verified locally:` / `*** aarch64-apple-darwin -> coverage owner today: CI job 'test' — Test (macos-latest) [native], job 'release-build' — Release Build (macos-latest)`; pre-audit printed **before** any leg; row `cross:check+clippy aarch64-apple-darwin FAIL (rust-std missing)`; `FAILURES: x target aarch64-apple-darwin is listed local|both … cfg class unverified`; `VERDICT: FAIL — 1 leg(s) red.` + `Gate fails closed: …` | **1** |
| 3 | `./scripts/cross_platform_check.sh --only-host` | exit code + explicit warning that no cross leg ran | `legs executed : 4 (PASS 4 / FAIL 0)   skipped-by-design rows: 0`, yet summary states `! --only-host was passed: NO cross-target leg ran. Windows and Apple cfg classes are entirely unverified in this run.` Fail-closed std accounting correctly does not apply (documented in `usage`). See R7 for the exit-0 caveat (`--only-host` is not the gate of record) | **0** |
| 3b | negative control: `--allow-missing-target x86_64-pc-windows-gnu --allow-missing-target aarch64-apple-darwin` | tolerance must not be abusable to skip an *installed* target | both `cross:check` and `cross:clippy` for `x86_64-pc-windows-gnu` still **ran and passed** (1s / 5s); `NOTE: --allow-missing-target x86_64-pc-windows-gnu had no effect: its rust-std is installed…` | **0** |
| 4a | `cargo check --all-targets --all-features --target x86_64-pc-windows-gnu 2>&1 \| tail -n 8` (**fresh**) | exit 0, 0 warnings | `Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)` / `Finished \`dev\` profile [unoptimized + debuginfo] target(s) in 11.16s`; `warning-lines: 0`, `error-lines: 0` | **0** |
| 4b | `cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings 2>&1 \| tail -n 8` (**fresh**) | exit 0, 0 warnings/errors | `Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)` / `Finished \`dev\` profile … in 12.79s`; `warning-lines: 0`, `error-lines: 0` | **0** |
| 4c | `cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu 2>&1 \| grep -c "^warning"` (**no** `-D warnings` flag) | `0` | `0` (pipeline cargo exit 0 as well) | **0** |
| 4d | `cargo check -v 2>&1 \| grep -o -- "--deny=[a-z_]*" \| sort -u` | denied lints visible | `--deny=clippy`, `--deny=dead_code`, `--deny=unused`, `--deny=unused_imports`; broader pattern `[a-z_:]*` yields `--deny=clippy::all` plus the three rustc lints. No `--allow=`/`--warn=` injected | **0** |
| 5 | `python3` + PyYAML parse of `.github/workflows/ci.yml` (job ids, display names, run commands) + `git show HEAD:.github/workflows/ci.yml` comparison | exactly 4 jobs; `test` has `components: clippy` + fmt + clippy steps; `cross-check` installs mingw and runs BOTH legs for the target resolved from `config/gate_targets.txt`; `release-build` unchanged | `JOB COUNT: 4`; `lint` = *Lint & Format* (identical to HEAD); `test` = *Test (${{ matrix.os }})*, `with: {targets: matrix.target, components: clippy}`, steps `Check code formatting` + `Run Clippy for this target` added by the diff; `cross-check` = *Cross-target check (windows-gnu)*, `needs: lint`, `sudo apt-get install -y gcc-mingw-w64-x86-64`, then `Run Clippy for the cross target` **and** `Check all targets for the cross target`; `release-build` = *Release Build (${{ matrix.os }})* — `identical=True` vs HEAD | **0** |
| 5b | CI's `awk` resolver from `ci.yml` executed locally against the real `config/gate_targets.txt` | resolves exactly one cross triple | `awk_exit=0 resolved=[x86_64-pc-windows-gnu] count=1` (so the `count -ne 1` guard would not fire; the job is not silently skipped) | **0** |
| 6 | Gate output vs the four triples in `config/gate_targets.txt` (line 56–57 … i.e. `x86_64-unknown-linux-gnu both`, `x86_64-pc-windows-gnu both`, `aarch64-apple-darwin both`, `x86_64-pc-windows-msvc ci`) | each unexercisable gap named and mapped to the right CI job | see `## Coverage gaps and their CI owners` — all four owner strings match the job ids/display names parsed from `ci.yml` in check 5 | **0** |
| 7 | Test-leg content of check 1 (grep of `run1_full.log`) | `--test-threads=1` + the 6 documented skips, 0 failures | 18 `Running` blocks + 1 doc block; **958 passed / 0 failed / 0 ignored / 6 filtered out**; lib block `814 passed; 0 failed; 5 filtered`, `tests/test_role_gating.rs` `24 passed; … 1 filtered`; the 6 filtered names are exactly the 6 `--skip` entries the script emits | **0** |
| 8 | Doc-test leg content | leg green | `Doc-tests marmennill` → `running 0 tests` → `test result: ok. 0 passed; 0 failed` — green but **vacuous** (see R8) | **0** |
| 9 | Parallel probe (CI parity for the test step, no `--test-threads`): `cargo test --lib` ×2, `cargo test --lib config::tests` ×10, `cargo test --lib config::tests::sandbox` ×5 | evidence for the residual-risk assessment, not a contract item | `--lib` parallel: run1 `811 passed; 8 failed` (exit 101), run2 `809 passed; 10 failed` (exit 101). `config::tests` filtered ×10 at default threads: `23 passed; 0 failed` every time. `config::tests::sandbox` ×5: `6 passed; 0 failed` every time | 101 / **0** / **0** |

### How the "fresh compile" was forced (check 4a/4b)

Two mechanisms, both content-preserving:

1. `cargo clean -p marmennill` — deleted **8584 files / 8.1 GiB** of *local-crate* artifacts while keeping every
   dependency unit fresh (252 of them report `Fresh` in the `-v` log; so the leg is a genuine cold analysis of the
   crate, not a cached hit, without a 10-minute dependency rebuild).
2. `CARGO_INCREMENTAL=0` on both legs — disables the incremental cache, so no prior incremental session can supply
   the result.

Corroboration that nothing was cached: a separate `-v` run (`c4_check.log`) after only `touch`-ing every `*.rs` in
`src/` and `tests/` (mtime only; md5 content verified identical) reported
`Dirty marmennill v1.0.0 (/home/fredrik/marmel/wip): the file \`src/harness/monitor/mod.rs\` has changed` and issued
**20 `Running \`rustc … --target x86_64-pc-windows-gnu\` invocations** (lib, lib-test harness, bin, all 16 integration
test targets); every dependency line in that log reads `Fresh`. After the `clean -p` + `CARGO_INCREMENTAL=0` runs the
cargo output literally reads `Checking marmennill v1.0.0` (11.16 s / 12.79 s), which cargo only prints for a unit it
actually re-analyzed. The two legs therefore cannot be cache hits.

### `[lints]` deny tables are active without `-D warnings`

`cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu` (no `-D warnings`) → `grep -c "^warning"` = **0**, and
`cargo check -v` shows the flags cargo injects from `Cargo.toml`: `'--deny=clippy::all' --deny=unused_imports --deny=unused --deny=dead_code`
(visible verbatim in the `c4_check.log` rustc command lines, and via the `grep -o -- "--deny=…"` extraction).
`Cargo.toml` `[lints.rust] unused|dead_code|unused_imports = "deny"` and `[lints.clippy] all = { level = "deny", priority = -1 }`
are therefore load-bearing on the cross target as well, exactly as the CI comment in the `test` job claims.

## Coverage gaps and their CI owners

Every one of these lines is printed by the gate itself in run 1 (quoted from `run1_full.log`), and each owner string was
cross-checked against the job ids / display names parsed from `.github/workflows/ci.yml` (check 5). No gap is silent.

| Triple (`config/gate_targets.txt`) | Scope | Exercised on this box? | Gap as NAMED by the gate output | CI owner named by the gate | Verified against `ci.yml`? |
|---|---|---|---|---|---|
| `x86_64-unknown-linux-gnu` | `both` | yes — host legs only (no redundant cross leg) | `x86_64-unknown-linux-gnu is the host triple — covered by the host legs (fmt, clippy --all-targets --all-features -D warnings, full test suite, doc tests) …` | `CI job 'test' — Test (ubuntu-latest), plus job 'lint' — Lint & Format` | ✅ `test` matrix entry `os: ubuntu-latest, target: x86_64-unknown-linux-gnu`; `lint` runs `cargo fmt --all -- --check` + `cargo clippy --all-targets --all-features -- -D warnings` |
| `x86_64-pc-windows-gnu` | `both` | **yes** — both cross legs green, twice (gate + fresh re-verification) | *no gap row: it is checked locally* | (owner printed for completeness) `CI job 'cross-check' — Cross-target check (windows-gnu), gated by 'lint' (steps 'Run Clippy for the cross target' + 'Check all targets for the cross target', triple resolved from config/gate_targets.txt)` | ✅ exact step names exist; `needs: lint`; mingw installed in the job; resolver returns exactly this triple (check 5b) |
| `aarch64-apple-darwin` | `both` | **no** — `rust-std` not installed, `~/.rustup` read-only ⇒ uninstallable | `aarch64-apple-darwin not installable locally — rust-std not installed (rustup cannot install any target on this box: ~/.rustup is read-only …) — tolerated only because --allow-missing-target aarch64-apple-darwin was passed — covered by …` + row `SKIPPED (std missing)` + pre-audit `COVERAGE: aarch64-apple-darwin rust-std is not installed — explicitly tolerated via --allow-missing-target.` | `CI job 'test' — Test (macos-latest) [native], job 'release-build' — Release Build (macos-latest)` | ✅ `test` matrix `os: macos-latest, target: aarch64-apple-darwin`; `release-build` matrix same |
| `x86_64-pc-windows-msvc` | `ci` | no — by design (Linux cross-compile of msvc is proven red: `onig_sys`/`aws-lc-sys`) | `x86_64-pc-windows-msvc scope=ci by design — cross-compiling it from Linux is proven red (onig_sys + aws-lc-sys build scripts …) — covered by …` + row `NOT-CHECKED (scope=ci)` | `CI job 'test' — Test (windows-latest) [native], job 'release-build' — Release Build (windows-latest)` | ✅ `test` matrix `os: windows-latest, target: x86_64-pc-windows-msvc` |
| 5 PTY-blocked + 1 state-sensitive test (skip list) | n/a | not runnable / not runnable reproducibly | `6 tests are skipped locally, split across three reasons …: 4 are PTY-device-blocked (/dev/pts/ptmx is mode 000 here, so openpty fails EACCES) and test_prompt_based_tool_gating is PTY-path-blocked …; the 6th, test_harness_delegate_task_manager_routes_and_returns_deliverable, is NOT PTY-dependent but state-sensitive/flaky — it fails at src/harness/mod.rs:1261 only when a .marmel/ session dir exists …` | `The 5 PTY-blocked ones run natively in the CI job 'test' legs: Test (ubuntu-latest), Test (macos-latest), Test (windows-latest).` | ✅ three `test` matrix legs exist |
| Behavioural parity (anything that needs linking/executing) | n/a | never — `check`/`clippy` do not link or run | `check/clippy do not link and no target binary is executed: behavioural parity (PTY, crossterm/console, landlock, process groups, path separators) remains CI-only` | implicit (`CI-only`) → the `test` matrix jobs | ✅ `test` runs `cargo build --all-targets`, `cargo test --all-targets --all-features --verbose` on all three OSes |

Additional independent confirmation of the two environment classifications (parallel probe `par1.log`, which fails on this box for
the same reasons the skip list documents): 4 PTY tests panic with `failed to openpty: Os { code: 13, kind: PermissionDenied }`
at `src/harness/pty_tests.rs:16`, `:102`, `:120`, `:179`, and the state-sensitive one panics at
`src/harness/mod.rs:1261:9: assertion failed: !res.is_error` (function declared at `src/harness/mod.rs:1251`) — i.e. the
skip list's citations are accurate today, and the `.marmel/` sensitivity is **live on this box** (`.marmel/` is present).

## Gate/CI mismatches

**None that affect the verdict.** The gate's target set, the per-triple scopes in `config/gate_targets.txt`, the CI job
set (`lint`, `test`, `cross-check`, `release-build`), the CI matrix targets and the gate's `ci_owner()` mapping agree
in both directions:

* every `local|both` triple either has an executed local leg (`x86_64-pc-windows-gnu`) or is the host triple
  (`x86_64-unknown-linux-gnu`) or is named as an explicitly tolerated gap (`aarch64-apple-darwin`);
* the single `ci`-scoped triple (`x86_64-pc-windows-msvc`) is exactly the triple CI covers natively on
  `windows-latest`, and neither CI nor the gate tries to cross-compile it from Linux;
* CI's `cross-check` resolver picks the same single triple the gate iterates (`check 5b`), and fails closed
  (`exit 1`) if the list yields ≠ 1 hit — the same fail-closed philosophy as the script;
* `release-build` is byte-identical to `git show HEAD:.github/workflows/ci.yml` (unchanged, as required); `lint` is
  also unchanged; the diff is confined to the `test` job (2 added steps + `components: clippy`) and the new
  `cross-check` job.

Advisory observations (documentation/CI hardening, none invalidating) are listed as non-blocking recommendations below.

## What the gate does NOT guarantee

1. **No test execution on macOS or Windows from this box — only typecheck/lint/cfg-branch parity.** The local cross
   legs are `cargo check` + `cargo clippy` (they do not link, and no target binary is executed). Compile-level parity
   for the `windows` cfg class is real (both legs green, freshly re-verified); *runtime* parity for
   `apple-darwin`/`windows-msvc` is guaranteed only by CI's native `test` matrix jobs, which cannot be executed or
   observed from this box. This audit verified `ci.yml` **statically** (PyYAML parse + running the job's own `awk`
   resolver locally) — no actual CI run was available, so "CI is green" is unverified, not assumed green.
2. **MCP-over-stdio behaviour is untested on Windows.** The scripted MCP server is POSIX shell
   (`sh -c "cat … ; sleep 30"`), and the 15 `#[cfg(unix)]` gates added by this change set (measured on the
   working-tree diff: `src/harness/tool_resolution_tests.rs` 10, `src/llm/client_tests.rs` 3, `src/config.rs` 1,
   `src/mcp/client.rs` 1; `docs/fix_verification.md` C2/C7 record 14 and miss `src/mcp/client.rs` — see R6)
   mirror the pre-existing unix-only tests. Consequence
   (already recorded as C2 in `docs/fix_verification.md`): on Windows there is no coverage of `mcp::client`
   wire-body/transport tests, the `tool_resolution_tests` MCP-path dispatch, or the `llm::client_tests` MCP schema
   test — only compile-level parity of those modules.
3. **`--test-threads=1` is used locally but is absent from CI's `test` job.** Assessment for the env-race fix that
   just landed: **the fix itself is thread-count independent and holds.** `src/config.rs:583`
   (`pub(crate) static SANDBOX_ENV_TEST_SERIAL: Mutex<()>`, taken by `sandbox_env_test_lock()` at
   `src/config.rs:594` inside `ActiveConfigTestGuard`/`SandboxEnvGuard`) serializes the `MARMEL_DISABLE_SANDBOX`
   `set_var`/`remove_var` writes and the `ACTIVE_CONFIG` writes *inside `config.rs` tests*, which is the Site-1 race
   that CI's parallel default exposed. Measured: `cargo test --lib config::tests` at default threads 10/10 runs
   (`23 passed; 0 failed`) and `cargo test --lib config::tests::sandbox` 5/5 (`6 passed; 0 failed`); in two full
   parallel `--lib` runs (8 and 10 failures) **no `config::tests::*` failure appeared**. So the absence of
   `--test-threads=1` in CI is *not* a risk for this specific fix.
   **But it is a real risk for the rest of the isolation debt:** full-lib parallel runs on this box reproduced the
   documented cascades — stably `harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler`,
   `harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool`
   (`there is no reactor running…` → `LazyLock instance has previously been poisoned`) and
   `tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool`, plus in one of two runs
   `orchestrator::notice_tests::test_handle_reply_to_arbitrator_success_and_validation` and
   `…::test_steering_history_multiple_notices_resolved_out_of_order`. CI runs this same lib binary with default
   threads and no skips, so those 3–5 failures are live CI flakiness candidates (`release-build` has `needs: test`,
   so flakiness there blocks releases). The local gate hides them by construction.
   Concrete contributing defect found here: `src/config.rs:578-580` states the lock is `pub(crate)` "so the other test
   modules that mutate `ACTIVE_CONFIG` (`src/orchestrator/notice_tests.rs`, `src/harness/dispatch_integrity_tests.rs`)
   can take the same lock", but grep shows **no call to `sandbox_env_test_lock`/`SANDBOX_ENV_TEST_SERIAL` anywhere
   outside `src/config.rs`**; those modules serialize only through `orchestrator::notice::TEST_NOTICE_MUTEX` while
   still calling `crate::config::set_active` (`src/orchestrator/notice_tests.rs:403,508,607`;
   `src/harness/dispatch_integrity_tests.rs:47,58` via its own `ActiveConfigGuard`). The `ACTIVE_CONFIG` writers are
   therefore still mutually unordered across modules (the `RwLock` makes it memory-safe, not logically isolated) —
   consistent with the two `notice_tests` failures seen in run 2.
4. **`.marmel/`-session-dir state sensitivity is masked, not fixed.** `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable`
   is in the skip list and fails at `src/harness/mod.rs:1261` iff a `.marmel/` session dir exists in the crate root
   (`docs/fix_verification.md` §B 2×2 matrix; `.marmel/` is gitignored, so CI never has one). Re-confirmed today: the
   test fails on this box (`.marmel/` present) and is skipped by the gate. Net effect: this failure mode is invisible
   **locally (skipped) and in CI (dir absent)** — the only place it can bite is a developer's dirty working tree, and it
   is mislabelled as "PTY-dependent" in `docs/cross_platform_gating.md` item 5. Needs its own ticket.
5. **Doc-test legs are vacuous.** `cargo test --doc --all-features` reports `running 0 tests`; CI's `cargo test --doc`
   likewise. A green doc-test row currently proves nothing.
6. **Malformed-list fail-closed behaviour of the parser was not exercised end-to-end** (it would require editing
   `config/gate_targets.txt`, out of scope for this verification-only task). Verified by inspection of the two
   independent parsers (script `while read` loop + CI `awk`), both of which hard-error on `NF != 2` / unknown scope,
   and by check 5b (resolver returns exactly 1 hit on the real list).
7. **`--only-host` returns 0** even though zero cross-target coverage exists. The summary warns loudly (verified in
   run 3), and the usage text says "never as the gate of record", but the exit status alone cannot distinguish it
   from a real gate pass.

## Recommendations

* **R1 · non-blocking (high priority) — close the CI parallel-test gap.** Either add `-- --test-threads=1` to the CI `test`
  job's `cargo test --all-targets --all-features --verbose` step, or fix the isolation debt (process-global MCP
  registry `LazyLock`/`MCP_GATE` poisoning via `REGISTRY_LOCK` discipline + `PtyManager::new()` `tokio::spawn`) and
  make `--test-threads=1` unnecessary. Today the local gate and CI disagree on thread count, so 3–5 parallel-mode
  failures visible on this box can surface as CI redness, and `release-build` depends on `test`.
* **R2 · non-blocking (high priority) — make the `ACTIVE_CONFIG` lock claim true.** `src/config.rs:578-580` asserts that
  `src/orchestrator/notice_tests.rs` and `src/harness/dispatch_integrity_tests.rs` take `SANDBOX_ENV_TEST_SERIAL`;
  they do not. Either have their `ActiveConfigGuard`/`set_active` call sites take `sandbox_env_test_lock()` (and expose
  a `pub(crate)` guard helper so they can), or correct the comment and file the ticket. This is the mechanism behind
  the `notice_tests` drift observed in parallel run 2.
* **R3 · non-blocking — add `rustfmt` to the `test` job's toolchain components.** The diff added a `cargo fmt --all -- --check`
  step to `test`, but that job requests only `components: clippy`. `dtolnay/rust-toolchain@stable` installs a minimal
  profile plus the requested components, so the new step depends on rustfmt being present by accident of the runner
  image. `lint` already lists `rustfmt, clippy`; mirror it (verification caveat: CI runner behaviour cannot be
  executed from this box).
* **R4 · non-blocking — fix script/CI command drift.** The script header (`scripts/cross_platform_check.sh:22`) documents
  `cross:clippy  cargo clippy --all-targets --target T -- -D warnings` while the implementation *and* CI both use
  `--all-targets --all-features --target T -- -D warnings`; conversely the gate runs `cargo test --doc --all-features`
  while CI's `test` job runs plain `cargo test --doc`. Align the three so "one shared list, one parity contract" stays
  literally true.
* **R5 · non-blocking — the `aarch64-apple-darwin` `both` scope is aspirational on this box.** The judgement call is
  defensible and honestly executed (the scope makes the gate *attempt* the triple and fail closed, which is why run 2
  exits 1 instead of being green by default; the green path requires naming the gap with
  `--allow-missing-target aarch64-apple-darwin`, verified in run 1). But a reader of `config/gate_targets.txt` alone
  would believe the `apple-darwin` cfg class has local coverage. Consider a fourth scope (e.g. `attempt`/`both-attempt`)
  or a comment-level `# local: impossible on <box>, std read-only` so the label matches reality.
* **R6 · non-blocking — update stale counts in the gate documentation.** `scripts/cross_platform_check.sh` header cites
  "measured exit 0 / 954 passed"; the observed figure today is **958 passed / 6 filtered** across 18 test-result
  blocks (matches `docs/fix_verification.md` check 8 and its C8 drift note). Same drift for the `#[cfg(unix)]` gate count: C2/C7 say 14, the working tree adds 15 (they omit `src/mcp/client.rs`).
* **R7 · non-blocking — give `--only-host` a distinguishable signal** (e.g. exit 0 only when `CI` is unset, a
  `GATE: NOT-OF-RECORD` marker line, or a machine-readable `--json` summary), so no automation can mistake run 3's
  exit 0 for the gate of record.
* **R8 · non-blocking — make the doc-test legs meaningful** (add at least one doc test) or drop the doc-test row from the
  acceptance contract; `running 0 tests` currently reads as coverage.
* **R9 · non-blocking — file the `.marmel/` ticket** referenced by coverage concern C1: `dispatch_for(TOOL_DELEGATE_TASK,
  Manager)` errors when `.marmel/execution_plan.md` is present in CWD. Until fixed, keep the skip but keep the
  "state-sensitive, NOT PTY" label (the script already labels it correctly; `docs/cross_platform_gating.md` item 5
  still mislabels it).

## Reproduction commands (as executed)

```bash
# gate, of record
./scripts/cross_platform_check.sh --allow-missing-target aarch64-apple-darwin   # exit 0
./scripts/cross_platform_check.sh                                               # exit 1 (fail closed)
./scripts/cross_platform_check.sh --only-host                                   # exit 0 (not of record)
./scripts/cross_platform_check.sh --allow-missing-target x86_64-pc-windows-gnu \
    --allow-missing-target aarch64-apple-darwin                                 # exit 0 + NOTE (no-op tolerance)

# independent, forced-fresh cross legs
cargo clean -p marmennill
CARGO_INCREMENTAL=0 cargo check  --all-targets --all-features --target x86_64-pc-windows-gnu
CARGO_INCREMENTAL=0 cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings
CARGO_INCREMENTAL=0 cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu 2>&1 | grep -c "^warning"   # 0
CARGO_INCREMENTAL=0 cargo check -v 2>&1 | grep -o -- "--deny=[a-z_]*" | sort -u

# CI parity (static) + resolver dry-run
python3 -c "import yaml,sys; d=yaml.safe_load(open('.github/workflows/ci.yml')); print(len(d['jobs']), list(d['jobs']))"
git show HEAD:.github/workflows/ci.yml > /tmp/gatever/ci_head.yml   # release-build identical=True

# CI-parallel test-behaviour probes (not part of the gate)
cargo test --lib                      # 811 passed / 8 failed ; 809 passed / 10 failed
cargo test --lib config::tests        # 23 passed / 0 failed  x10
cargo test --lib config::tests::sandbox   # 6 passed / 0 failed  x5
```
