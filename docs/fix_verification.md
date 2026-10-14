# t-004 — Independent host-side verification of the landed cross-platform fixes

Auditor: validator specialist (read-only audit; no source file modified — only `mtime`
touches to bust cargo caches, plus scratch copies under `/tmp/baseline`, `/tmp/fixed`,
`/tmp/baseline_target`, `/tmp/fixed_target`).
Host: Linux x86_64, `x86_64-pc-windows-gnu` rust-std + `/usr/bin/x86_64-w64-mingw32-gcc` installed.
CWD: `/home/fredrik/marmel/wip` (crate `marmennill`), git working tree dirty at `9367552`.

## Verdict: PASS

All 9 acceptance checks pass with real, quoted output. No real regression was found. Two
non-green local failures (`harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable`,
`test_prompt_based_tool_gating`) plus 4 PTY tests are environment/state-blocked and reproduce
**identically on the pristine pre-fix tree** (proof in `## Remaining failures classified` §B).

## Checks

| # | Command | Expected | Observed | Exit |
|---|---|---|---|---|
| 1 | `cargo fmt --all -- --check` | exit 0 | no output | `0` |
| 2a | `find src tests -name '*.rs' -exec touch {} +` then `cargo clippy --all-targets --all-features -- -D warnings` (forced fresh: output contains `Checking marmennill v1.0.0`, 4.70 s) | exit 0, **zero** diagnostics | `Checking marmennill v1.0.0 (…)` / `Finished dev profile … in 4.70s` — no `warning`/`error` line at all | `0` |
| 2b | `cargo clippy --all-targets --all-features 2>&1 \| grep -E "sandbox.rs\|mcp/client.rs\|tool_resolution_tests.rs\|client_tests.rs" \| grep -E "warning\|error"` | empty | empty (`GREP_LINES=0`; `grep_exit=1` = no match) | `1` (no match = success) |
| 2c | `cargo check --all-targets --all-features -v` (fresh) — proves the new `[lints]` tables are actually handed to rustc | deny flags present | `--deny=dead_code`, `--deny=unused`, `--deny=unused_imports` (2 units each) | `0` |
| 2d | `cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings` (fresh after touch: `Checking marmennill`, 4.59 s) | exit 0, 0 diagnostics | `Checking marmennill v1.0.0 (…)` / `Finished … in 4.59s`; `warnings/errors: 0` | `0` |
| 3a | `cargo test --bin marmel test_canonical_sandbox_cwd -- --exact tests::test_canonical_sandbox_cwd_resolves_symlinks` | pass | `test tests::test_canonical_sandbox_cwd_resolves_symlinks ... ok` / `test result: ok. 1 passed; 0 failed; … 7 filtered out` | `0` |
| 3b | `cargo test --bin marmel` | all pass | `test result: ok. 8 passed; 0 failed; 0 ignored` | `0` |
| 4 | `for i in 1 2 3 4 5; do cargo test --lib config::tests::sandbox 2>&1 \| tail -n 2; done` | 0 failures ×5 | ×5 → `test result: ok. 6 passed; 0 failed; 0 ignored; 0 measured; 813 filtered out`; `sandbox_opt_out_env_round_trip_is_explicit_and_restored ... ok` present in **all 5** run lists | `0` ×5 |
| 4b (extra stress) | `cargo test --lib config::` ×3 at **default (parallel) threads**, plus `cargo test --lib` ×3 parallel | no env race | ×3 → `test result: ok. 23 passed; 0 failed`; 3 parallel full-lib runs → **zero** `config::tests::*` failures | `0` ×3 |
| 5 | `cargo test --lib harness::search` | all pass, incl. the named test | `test result: ok. 15 passed; 0 failed` incl. `harness::search::tests::normalize_for_compare_is_identity_on_unix_and_canonical_on_windows ... ok` | `0` |
| 6a | `cargo test --lib mcp::` | all pass | `test result: ok. 54 passed; 0 failed; 0 ignored; 765 filtered out` | `0` |
| 6b | `cargo test --test test_tool_name_literals` | pass | `test test_no_unexpected_raw_tool_name_literals_in_src ... ok` / `1 passed; 0 failed` | `0` |
| 7 | `cargo test --lib harness::pty -- --exact harness::pty::tests::test_build_sandboxed_command_logs_its_choice_and_keeps_the_wrapper` | pass | `test harness::pty::tests::test_build_sandboxed_command_logs_its_choice_and_keeps_the_wrapper ... ok` / `1 passed; 0 failed; 818 filtered out` | `0` |
| 8 | `cargo test --all-targets --all-features --no-fail-fast -- --test-threads=1 --skip <6 documented names>` | totals reported, no real failure | **958 passed / 0 failed / 6 filtered** across 18 test-result blocks (lib `814 passed; 0 failed; 5 filtered`, bin `marmel` 8, 16 integration suites 24/21/15/14/10/9/8/8/8/6/5/4/4/3/2/1). Full block list in `## Commands for reproducibility` | `0` |
| 8b | `scripts/cross_platform_check.sh --allow-missing-target aarch64-apple-darwin` (the sanctioned recipe end to end) | gate green, gaps named | `host:fmt PASS 1s`, `host:clippy PASS`, `host:test PASS 151s`, `host:doc-test PASS 2s`, `cross:check x86_64-pc-windows-gnu PASS 3s`, `cross:clippy x86_64-pc-windows-gnu PASS`; `cross:check+clippy aarch64-apple-darwin SKIPPED (std missing)`, `x86_64-pc-windows-msvc NOT-CHECKED (scope=ci)`; `VERDICT: PASS — 6 leg(s) green, 3 coverage gap row(s) explicitly accounted for`; test total summed from the leg = **958** | `0` |
| 9 | diff review (`git diff`) for deleted assertions / over-broad `#[cfg(unix)]` | statement, see `## Production-behaviour review` | 0 test attributes deleted, 4 tests added, 14 `#[cfg(unix)]` gates added (brief says 12), windows-target `--all-targets` clippy clean | n/a |

## Production-behaviour review (check 9)

**Only three production (non-`#[cfg(test)]`) code paths changed** — everything else in the diff is
test code, `Cargo.toml` lint tables, `scripts/`, `config/`, `docs/`:

1. `src/harness/search.rs:297` (call site) + `:504`/`:520` (`NATIVE_PATH_SEPARATOR` + `normalize_glob_path()`),
   applied at the `glob_in_root` call site. On a `/`-based platform the function returns
   `rel.to_owned()` (byte-identical), so Unix/macOS behaviour is provably unchanged; on Windows it
   re-renders the walker's `\` spelling into the `/` spelling that `glob_to_regex` matches. This is a
   strengthening (a `/`-spelled pattern previously matched **nothing** on Windows).
2. `src/config.rs:536-563` — `home_dir()` rewritten from `.or_else(|| {#[cfg(unix)] … #[cfg(not(unix))] None})`
   into `if let Some(home) = … { return … } ; #[cfg(unix)] {…} ; #[cfg(not(unix))] { None }`.
   Semantics identical (`HOME` first, passwd fallback second). The only motivation is the
   windows-only `clippy::unnecessary_lazy_evaluations` that a `-D warnings` cross leg would hit.
3. `Cargo.toml:64-73` — `[lints.rust] unused/dead_code/unused_imports = "deny"`, `[lints.clippy] all = deny`.
   Verified live (check 2c). No `#[allow]` was added anywhere: `grep -c "^[+-].*#\[allow" git diff` → 0.

**Were any assertions deleted rather than made platform-aware? — No. Zero.**
`git diff | grep -E '^\-' | grep -E 'assert|expect\(|unwrap'` yields exactly 7 removed lines, every one
of them re-issued in an equal-or-stronger, platform-aware form:

| Removed | Replacement |
|---|---|
| `src/main.rs:384` `assert_eq!(canonical_sandbox_cwd(&link.to_string_lossy()), real)` | `assert_eq!(resolved, std::fs::canonicalize(&real).unwrap())` **plus a new** `assert_ne!(resolved, link)` — strictly stronger (also non-vacuity) |
| `src/harness/pty_tests.rs` `assert_eq!(argv[0]…,"sh")`, `assert_eq!(argv[1]…,"-c")` | `expected_sandbox_shell()` (`("cmd","/C")` on Windows, `("sh","-c")` on Unix) — verified against production `src/harness/pty.rs:764-781` |
| `src/harness/pty_tests.rs` `logs.contains("WARN") && logs.contains("pty sandbox decision")` | same `WARN` plus a platform-chosen marker (`"pty sandbox decision"` unix / `"SKIPPED"` windows, both strings exist in `pty.rs:674` / `:768`) |
| `src/harness/pty_tests.rs` `wrapped.starts_with("stty -echo …")`, `wrapped.ends_with("echo hi")` | kept verbatim under `else` (Unix); Windows branch asserts `payload == "echo hi"`, matching `pty.rs:778-780` (`cmd /C <command>`, no POSIX preamble) |
| `src/mcp/http_tests.rs` `production_only` = `src.split("#[cfg(test)]\nmod tests {").next().unwrap_or(src)` | CRLF-normalized, anchor-free cut (`find("mod tests")` ∪ `cfg_gated_module`) that **fails closed** with `assert!` instead of `unwrap_or(src)`, plus positive `protocol::initialize(`/`list_tools(`/`call_tool(` seam assertions and 2 new tests (one `#[should_panic]`) |
| `src/config.rs` `SandboxEnvGuard::set("")` ×3 + `opt_out_from_env()` in three tests | `opt_out_from_value(Some(""))` / `Some("1")` / `Some("0")` / `None` pure-parser assertions + new `merge_sandbox_opt_out` call; the *env* half is now covered by the dedicated round-trip test (see coverage concern C3) |
| `src/harness/tool_resolution_tests.rs` 2 merged `use` lines | split into `#[cfg(unix)]`-gated and ungated imports (no assertion involved) |

`#[test]`/`#[tokio::test]` attributes removed: **0**; added: **4** (`sandbox_opt_out_env_round_trip_is_explicit_and_restored`,
`default_config_keeps_the_builtin_command_timeout`, `production_only_cut_survives_crlf_line_endings`,
`production_only_refuses_to_scan_a_file_whole`) + 1 `#[should_panic]`.

**Does any new `#[cfg(unix)]` gate hide a test that should also run on Windows? — No.**
The diff adds **14** gates (brief said 12): `src/mcp/client.rs` 1, `src/harness/tool_resolution_tests.rs` 10,
`src/llm/client_tests.rs` 3. Every one is on a **helper/`use`/`static`/`struct` item**, never on a `#[test]`
function, and every consumer was already `#[cfg(unix)]`-gated before this diff (e.g. `tool_resolution_tests.rs:153,202,281,449,514`;
`llm/client_tests.rs:687`; `mcp/client.rs:565,637,663,689,751,792,823` with the in-repo comments "Keeps the
`cfg(unix)` gate"). The gated machinery is POSIX-shell by construction (`tool_resolution_tests.rs:100-106`:
`command: "sh"`, `args: ["-c", "cat '<fixture>' ; sleep 30"]`). Machine proof that nothing was left dangling or
hidden on the Windows cfg class: fresh `cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu
-- -D warnings` → exit 0, 0 diagnostics (check 2d). Conversely the diff **removes** a linux-only gate
(`src/harness/sandbox.rs:644`), so `test_apply_sandbox_fails_closed_on_an_unusable_workspace_root` is now
declared for every target; that is sound, not vacuous: `apply_sandbox` (`sandbox.rs:157`) is ungated and the
non-linux `attempt_landlock` (`sandbox.rs:486-493`) returns `Unavailable`, which `apply_sandbox` logs as
`"Landlock sandbox NOT established"` — exactly the two things the test asserts.

Note: `.github/workflows/ci.yml` gained **+114 lines** *during* this audit window (it was not in the brief's
list; `git diff --stat` moved from 11 files / 658 insertions to 12 files / 772 insertions). Verified content:
the `Cross-target check (windows-gnu)` job resolves its triple from the shared `config/gate_targets.txt`
(lines 106-189), so that file's "read by BOTH" claim is true, and the `Test (ubuntu/macos/windows)` matrix runs
`cargo test --all-targets --all-features --verbose` natively.

## Remaining failures classified

Everything that is not green locally, with evidence and an isolation re-run.

### A. 4 × `harness::pty::tests::*` — environment-blocked (PTY devices)

Isolation re-runs (`cargo test --lib <name> -- --exact <name>`, each exit `101`):

```
src/harness/pty_tests.rs:16:47   spawn sandbox: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied })
src/harness/pty_tests.rs:120:10  spawn session: Execution(Failed to create PTY: failed to openpty: Os { code: 13, kind: PermissionDenied })
src/harness/pty_tests.rs:102:34  executes with custom timeout: Execution(failed to openpty: Os { code: 13, kind: PermissionDenied })
src/harness/pty_tests.rs:179:5   assertion failed: res.is_ok()      (the PTY spawn inside failed first)
```

Host cause (matches `docs/baseline_test_run.md` §6 and `docs/cross_platform_gating.md` "Pre-existing
env-dependent test failures"):

```
$ stat -c '%A %a %U:%G %n' /dev/pts/ptmx
c--------- 0 root:root /dev/pts/ptmx
$ python3 -c "import pty; pty.openpty()"
openpty FAILED: out of pty devices
```

None of `src/harness/pty.rs`, `src/harness/pty_tests.rs`'s PTY fixtures (`:16/:102/:120/:179`) or the
`/dev/pts` state changed in a way that could cause this; the same names/panics are recorded pre-change.

### B. `harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable` — blocked by repo session state (`.marmel/`), NOT by the fixes

Isolation re-run in the audited tree (`exit 101`):

```
thread 'harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable' panicked at src/harness/mod.rs:1261:9:
assertion failed: !res.is_error
```

Forensic 2×2 matrix (pristine `git archive HEAD` in `/tmp/baseline`, HEAD+`git diff` patch in `/tmp/fixed`,
both re-run with and without the untracked `.marmel/` session dir — `.marmel/` holds
`execution_plan.md`, `.session_transcript.json`, `marmel.log`):

| Tree | `.marmel/` in crate root | Result |
|---|---|---|
| HEAD (pre-fix) | absent | `test result: ok. 1 passed; 0 failed; 814 filtered out` |
| HEAD (pre-fix) | present | `FAILED … panicked at src/harness/mod.rs:1261:9` |
| HEAD + landed fix | present | `FAILED … panicked at src/harness/mod.rs:1261:9` (identical) |
| HEAD + landed fix | absent | `test result: ok. 1 passed; 0 failed; 818 filtered out` |

⇒ The failure is a function of the leftover marmel session state in the working directory, not of the audited
changes (`src/harness/mod.rs` is **not** in the diff; the panic line `mod.rs:1261` is byte-identical pre/post fix).
Classified as environment/state-blocked noise, with the caveat in coverage concern **C1** below: the reason
recorded in `docs/cross_platform_gating.md` (item 5, "PTY-dependent") is **not** the cause on this box.

### C. `tests/test_role_gating.rs::test_prompt_based_tool_gating` — environment-blocked (PTY devices)

Isolation re-run at the audited tree: `panicked at tests/test_role_gating.rs:312:5: Validator with run_command
in prompt must be permitted to execute run_command` (exit `101`). Reproduced **identically on the pristine
pre-fix tree** (`/tmp/baseline`: `test result: FAILED. 0 passed; 1 failed; 24 filtered out`, exit `101`), both
with and without the untracked files → pre-existing. Mechanism: `tests/test_role_gating.rs:296-312` calls
`dispatch_for(run_command {"command":"echo test"}, …)` and asserts `.is_ok()`; that dispatch is
`src/harness/mod.rs:337/683/1083 → pty::run_command`, which cannot `openpty` on this box (see §A).
Also sanctioned in `docs/baseline_test_run.md` §6 (`docs/cleanup_report.md:25`).

### D. Parallel-mode (`--lib` default threads) cascades — documented baseline noise, membership drifts

`cargo test --lib` ×3 on the audited tree (exit `101`, 16-17 s):

```
run 1: 809 passed; 10 failed
run 2: 811 passed;  8 failed
run 3: 808 passed; 11 failed
stable core: 4 × harness::pty (see §A) + harness::tests::test_harness_delegate… (see §B)
drifting:  harness::tool_resolution_tests::specialist_allowlists_admit_alias_spellings_of_the_granted_tool
             → panicked at src/harness/pty.rs:1162:9: there is no reactor running, must be called from the
               context of a Tokio 1.x runtime
           harness::tool_resolution_tests::aliases_that_resolved_before_still_reach_a_builtin_handler
           tool_names::tests::claimed_aliases_never_fall_through_to_unknown_tool
             → library/std/src/sync/lazy_lock.rs:415:5: LazyLock instance has previously been poisoned
           orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup
           orchestrator::notice_tests::test_steering_history_multiple_notices_resolved_out_of_order
             → src/orchestrator/notice_tests.rs:668:9 assertion failed: hist[0].1.contains("Coder has fixed the PPC JIT.")
           ui::bridge::tests::test_bridge_notice_capacity_drop_is_observable_and_inbox_stays_bounded   (1 of 3 runs)
           harness::sleep::tests::sync_and_async_sleep_paths_agree_on_every_input_shape                (1 of 3 runs)
             → src/harness/sleep.rs:412:21: async sleep shortened float 6.0 exact (…) to 6.002260648s; expected a 6s wait
```

Why this is noise and not a regression: (i) the membership drifts between runs while the total failure count
moves 8-11, exactly the pattern recorded in `docs/baseline_test_run.md` §5.3 (9-11 failures, drifting
membership) and `docs/cross_platform_gating.md` ("the other 5 parallel-mode failures are **flaky cascades**,
not separate bugs"); (ii) every implicated file — `src/harness/pty.rs`, `src/harness/mod.rs`,
`src/orchestrator/notice_tests.rs`, `src/tool_names*`, `src/ui/bridge/*`, `src/harness/sleep.rs` — is
**untouched by the diff**; (iii) the `LazyLock … poisoned` lines are the documented cascade of the
`no reactor running` panic over the process-global MCP registry (`REGISTRY_LOCK`/`MCP_GATE`); (iv) all of them
disappear under the sanctioned serialized recipe (958/0, check 8); (v) the `harness::sleep` line is a
wall-clock jitter assertion (6.002260648 s vs 6 s), environment-timing-dependent.
**No `config::tests::*` failure appeared in any of the 3 parallel full-lib runs or the 3 parallel
`config::` runs** — which is precisely what the env-race fix (check 4/4b) had to deliver.

## Real regressions

**None.** No acceptance check failed; every non-green test either (a) cannot run on this box
(`/dev/pts/ptmx` mode `000`), (b) reproduces byte-identically on the pristine pre-fix tree, or (c) is a
documented parallel-mode cascade in files the diff does not touch.

## Coverage concerns

* **C1 — the skip list masks a genuine repo-state sensitivity.** `test_harness_delegate_task_manager_routes_and_returns_deliverable`
  fails **iff** a `.marmel/` session dir exists in the crate root (proof: §B matrix). CI never has such a dir,
  so this failure mode is invisible both locally (skipped) and in CI. `docs/cross_platform_gating.md` item 5
  attributes it to PTY dependence and `docs/baseline_test_run.md` classifies it as a real defect — neither is
  the measured cause here. `scripts/cross_platform_check.sh:408` also labels all 6 skips "PTY-dependent tests",
  which is inaccurate for this one. Deserves its own ticket (`dispatch_for(TOOL_DELEGATE_TASK, Manager)`
  erroring when `.marmel/execution_plan.md` is present in CWD). Not a defect introduced by this change set.
* **C2 — MCP-over-stdio behaviour stays untested on Windows.** The 14 new `#[cfg(unix)]` gates correctly mirror
  pre-existing unix-only tests, but the consequence is that on Windows there is no coverage of
  `mcp::client` wire-body/transport tests, `tool_resolution_tests` MCP-path dispatch, or the
  `llm::client_tests` MCP schema test, because the scripted server is POSIX shell (`sh -c "cat … ; sleep 30"`).
  Only the compile-level Windows parity of those modules is guaranteed (check 2d).
* **C3 — env-contract coverage narrowed to two env values.** The "present but empty is not an opt-out" case is
  now asserted only through the pure parser (`opt_out_from_value(Some(""))`); `SandboxEnvGuard` additionally
  *forbids* an empty value by `assert!`. The env round-trip test covers `=1` and cleared. Justified (Windows
  `set_var(k,"")` deletes the key, so the env level cannot express it there), but on Unix/Linux the
  env-level empty-string case is no longer exercised.
* **C4 — `--test-threads=1` is load-bearing locally but absent in CI.** The local gate is serialized (that is
  what removes the poisoned-`LazyLock` cascades); the CI matrix runs `cargo test --all-targets --all-features
  --verbose` with default parallel threads and no skips, so the documented test-isolation weakness
  (process-global MCP registry + `tokio::spawn` in `PtyManager::new()`, `docs/cross_platform_gating.md`
  "worth its own fix ticket") remains a CI flakiness risk that this change set does not fix.
* **C5 — non-linux behaviour of the de-gated sandbox test is compile-verified only.**
  `test_apply_sandbox_fails_closed_on_an_unusable_workspace_root` now compiles for `x86_64-pc-windows-gnu`
  (and its two asserted strings do exist on that path), but the Windows branch of `apply_sandbox` is only
  *executed* by CI `Test (windows-latest)`.
* **C6 — Apple parity is CI-only on this box.** `aarch64-apple-darwin` rust-std cannot be installed
  (`~/.rustup` read-only), so the gate must be run with `--allow-missing-target aarch64-apple-darwin`; the
  script states that gap explicitly rather than hiding it (`SKIPPED (std missing)` row).
* **C7 — brief-vs-diff count mismatch:** the brief says 12 `#[cfg(unix)]` gates; the diff adds 14
  (1 + 10 + 3). Same intent, all helper-level.
* **C8 — test-count drift (informational):** lib target grew 811 → 819 tests (4 added tests + the split
  timeout test), hence "958 passed" here vs the 954 recorded in `docs/cross_platform_gating.md`.

## Commands for reproducibility

```bash
cd /home/fredrik/marmel/wip

# 1. format
cargo fmt --all -- --check                       # exit 0

# 2. clippy, cache-busted, host + windows cfg class
find src tests -name '*.rs' -exec touch {} +     # mtime only; content untouched
cargo clippy --all-targets --all-features -- -D warnings          # exit 0, no diagnostics
cargo clippy --all-targets --all-features 2>&1 \
  | grep -E "sandbox\.rs|mcp/client\.rs|tool_resolution_tests\.rs|client_tests\.rs" \
  | grep -E "warning|error"                                        # empty
cargo check --all-targets --all-features -v 2>&1 | grep -o -- '--deny=[a-z_]*' | sort -u
                                                   # --deny=dead_code --deny=unused --deny=unused_imports
cargo clippy --all-targets --all-features --target x86_64-pc-windows-gnu -- -D warnings  # exit 0, 0 diagnostics

# 3. the originally failing test + whole bin target
cargo test --bin marmel test_canonical_sandbox_cwd -- --exact tests::test_canonical_sandbox_cwd_resolves_symlinks
cargo test --bin marmel                            # 8 passed / 0 failed

# 4. env-race regression (fix must be stable, not lucky)
for i in 1 2 3 4 5; do cargo test --lib config::tests::sandbox 2>&1 | tail -n 2; done
cargo test --lib config::                          # ×3, default threads: 23 passed / 0 failed

# 5. search normalization
cargo test --lib harness::search                   # 15 passed / 0 failed

# 6. MCP scans
cargo test --lib mcp::                             # 54 passed / 0 failed
cargo test --test test_tool_name_literals          # 1 passed / 0 failed

# 7. PTY argv test
cargo test --lib harness::pty -- --exact harness::pty::tests::test_build_sandboxed_command_logs_its_choice_and_keeps_the_wrapper

# 8. sanctioned full suite (6 documented skips from scripts/cross_platform_check.sh:57-64)
cargo test --all-targets --all-features --no-fail-fast -- --test-threads=1 \
    --skip harness::pty::tests::test_harness_pty_process_group_kill \
    --skip harness::pty::tests::test_interactive_pty_manager_lifecycle \
    --skip harness::pty::tests::test_run_command_parses_timeout_seconds_argument \
    --skip harness::pty::tests::test_run_command_pty_cancelled_by_worker_token \
    --skip harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable \
    --skip test_prompt_based_tool_gating
# → 958 passed / 0 failed / 6 filtered; per-target blocks:
#   lib 814/0 (5 filtered) · bin marmel 8 · test_agent 2 · test_context 8 · test_dynamic_prompts 4 ·
#   test_harness 15 · test_llm 5 · test_midturn_notice 3 · test_monitor 2 · test_orchestrator 8 ·
#   test_plan_checkoff 6 · test_role_gating 24/0 (1 filtered) · test_specialist_schema_charge 4 ·
#   test_specialist_stream 10 · test_steer_history 9 · test_tool_name_literals 1 · test_ui_session 21 ·
#   test_validation_loop 14
cargo test --doc --all-features                    # 0 doctests

# 8b. the whole gate, end to end
scripts/cross_platform_check.sh --allow-missing-target aarch64-apple-darwin   # VERDICT: PASS, exit 0

# unsanctioned parallel view (documents the noise floor)
cargo test --lib                                   # ×3: 10/8/11 failed, drifting membership (§D)

# classify the 6 skips
ls -l /dev/pts/ptmx ; stat -c '%A %a %U:%G %n' /dev/pts/ptmx   # c--------- 0 root:root
python3 -c "import pty; pty.openpty()"             # OSError: out of pty devices
cargo test --lib harness::pty::tests::test_harness_pty_process_group_kill -- --exact harness::pty::tests::test_harness_pty_process_group_kill
cargo test --lib harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable -- --exact harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable
cargo test --test test_role_gating test_prompt_based_tool_gating -- --exact test_prompt_based_tool_gating

# the §B pre-fix control (no repo modification: HEAD exported to /tmp)
mkdir -p /tmp/baseline && git archive HEAD > /tmp/baseline.tar && tar -xf /tmp/baseline.tar -C /tmp/baseline
(cd /tmp/baseline && CARGO_TARGET_DIR=/tmp/baseline_target cargo test --lib harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable -- --exact harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable)
(cd /tmp/baseline && CARGO_TARGET_DIR=/tmp/baseline_target cargo test --test test_role_gating test_prompt_based_tool_gating -- --exact test_prompt_based_tool_gating)
mkdir -p /tmp/fixed && tar -xf /tmp/baseline.tar -C /tmp/fixed && git diff > /tmp/fix.patch
(cd /tmp/fixed && patch -p1 < /tmp/fix.patch) && cp -a .marmel /tmp/fixed/
(cd /tmp/fixed && CARGO_TARGET_DIR=/tmp/fixed_target cargo test --lib harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable -- --exact harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable)
(cd /tmp/fixed && rm -rf .marmel && CARGO_TARGET_DIR=/tmp/fixed_target cargo test --lib harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable -- --exact harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable)
```
