# Windows test failures — root cause + minimal-fix spec (t-006)

**Mode:** read-only analysis. No source file was modified. Every quoted block below is
verbatim from the working tree at HEAD `9367552` (`1.0.0-refactor`, dirty:
`src/harness/sandbox.rs`, `src/main.rs` modified by other tasks).

**Method / evidence used (all read-only):**

| Evidence | How obtained |
|---|---|
| CI job/OS/command facts | `cat .github/workflows/ci.yml` (line refs below) |
| Site 1 race | `./target/debug/deps/marmennill-25e1737d17c27a1e 'config::tests::sandbox'` ×40 at default threads, ×12 with `--test-threads=4`, ×5 with `--test-threads=1` |
| Site 3 separator asymmetry | the exact regexes emitted by `glob_to_regex` run against the exact `\`-spelled subjects Windows would produce |
| Site 4 CRLF mechanism | the exact `production_only` split + the exact negative assertions replayed over `src/mcp/client.rs` in LF and CRLF spelling |
| `argv[0]` facts | production code path `src/harness/pty.rs:756-794` + vendored `portable-pty-0.9.0/src/cmdbuilder.rs:215,290` |

---

# Windows CI: what actually runs (from `.github/workflows/ci.yml`)

Source of truth: `.github/workflows/ci.yml` (106 lines, single workflow `CI`, `on:
push|pull_request|workflow_dispatch`, `concurrency.cancel-in-progress: true`,
`env: CARGO_TERM_COLOR=always, RUST_BACKTRACE=1`).

| Job id | Display name | runs-on | Commands |
|---|---|---|---|
| `lint` (ci.yml:19) | `Lint & Format` | `ubuntu-latest` (ci.yml:21) | `cargo fmt --all -- --check` (ci.yml:35); `cargo clippy --all-targets --all-features -- -D warnings` (ci.yml:38) |
| `test` (ci.yml:40, `needs: lint`, `fail-fast: false`) | `Test (${{ matrix.os }})` | `${{ matrix.os }}` (ci.yml:53) | `cargo build --all-targets --all-features --verbose` (ci.yml:69); **`cargo test --all-targets --all-features --verbose`** (ci.yml:72); `cargo test --doc` (ci.yml:75) |
| `release-build` (ci.yml:77, `needs: test`, `fail-fast: false`) | `Release Build (${{ matrix.os }})` | `${{ matrix.os }}` (ci.yml:90) | `cargo build --release --verbose` (ci.yml:106) |

Matrix (`strategy.matrix.include`, identical in both matrix jobs):

- `os: ubuntu-latest`, `target: x86_64-unknown-linux-gnu` (ci.yml:47-48 / 84-85)
- `os: macos-latest`, `target: aarch64-apple-darwin` (ci.yml:49-50 / 86-87)
- **`os: windows-latest`, `target: x86_64-pc-windows-msvc` (ci.yml:51-52 / 88-89)**

CI facts that matter for the five failures:

1. **CI is `windows-msvc`, not `windows-gnu`.** Yes — `x86_64-pc-windows-msvc`
   (ci.yml:52, ci.yml:89). Every local Windows evidence in this repo is
   **`x86_64-pc-windows-gnu` and compile-only**: `docs/sandbox_deadcode_audit.md:6`
   and `docs/verification_final.md:153` (`cargo check --target x86_64-pc-windows-gnu
   --all-targets` → exit 0). There is no `cargo test --target x86_64-pc-windows-*`
   anywhere and no Wine execution step. **Consequence:** a green
   `cargo check --target x86_64-pc-windows-gnu --all-targets` proves *compilation of
   the `cfg(windows)` halves only*; it can never observe any of these five runtime
   assertion failures, and it cannot observe msvc-only differences (CRT environment
   semantics, `cmd.exe` resolution, link/ABI) either. Local reproduction of these
   five failures requires an actual Windows **run** (native runner or msvc target
   under a Wine runner), not a gnu compile check.
2. **The test step has no thread control:** `cargo test --all-targets
   --all-features --verbose` (ci.yml:72) → the lib test binary uses the libtest
   default of one thread per logical CPU (`windows-latest` = 4 vCPU → 4 threads).
   `--test-threads=1` appears **nowhere** in `.github/`; it only appears in local
   diagnostic recipes in `docs/` (`docs/baseline_test_run.md:34,321`,
   `docs/verification_final.md:44`). This directly invalidates the SAFETY comment in
   Site 1.
3. **`--all-targets` builds and runs lib tests, integration tests and bins in one
   job**; the lib binary contains all the process-global mutations discussed below.
4. **Checkout line endings:** `actions/checkout@v4` (ci.yml:55) with no
   `.gitattributes` in the repo (verified: `ls -a` → no `.gitattributes`; local
   `git config --get core.autocrlf` → `input`, i.e. LF in the working tree here) and
   no `core.autocrlf` override in the workflow. On Windows runners `actions/checkout`
   checks the tree out with CRLF by default, so `include_str!` embeds `\r\n` bytes in
   the `windows-latest` job. This is the trigger for Site 4.

---

## Site 1 — `src/config.rs:885-893`, test `sandbox_disabled_knob_reaches_sandbox_inputs_opt_out`

### Current code

`src/config.rs:781-815` (the helper, in `mod tests`):

```rust
    /// Set `MARMEL_DISABLE_SANDBOX` for one test and restore the environment
    /// afterwards.
    struct SandboxEnvGuard {
        previous: Option<std::ffi::OsString>,
    }

    impl SandboxEnvGuard {
        fn set(value: &str) -> Self {
            let previous = std::env::var_os(crate::harness::pty::SANDBOX_OPTOUT_ENV);
            // SAFETY: these tests are gated with `--test-threads=1`, and the key
            // is read by nothing else in this binary while it is set; the guard
            // restores the original state on drop.
            unsafe {
                std::env::set_var(crate::harness::pty::SANDBOX_OPTOUT_ENV, value);
            }
            Self { previous }
        }
    }

    impl Drop for SandboxEnvGuard {
        fn drop(&mut self) {
            // SAFETY: as above — single-threaded test run, restoring the previous
            // value of a key no other test in this binary depends on.
            unsafe {
                match self.previous.take() {
                    Some(previous) => {
                        std::env::set_var(crate::harness::pty::SANDBOX_OPTOUT_ENV, previous);
                    }
                    None => {
                        std::env::remove_var(crate::harness::pty::SANDBOX_OPTOUT_ENV);
                    }
                }
            }
        }
    }
```

The failing test, `src/config.rs:883-897`:

```rust
    /// (a) the knob reaches `SandboxInputs::opt_out` and therefore the decision.
    #[test]
    fn sandbox_disabled_knob_reaches_sandbox_inputs_opt_out() {
        let cfg = partial_from("sandbox_disabled = true\n");
        assert!(cfg.sandbox_disabled);

        let _guard = ActiveConfigTestGuard::install(cfg);
        // Env unset: the env parser contributes `None`, so only the knob can opt out.
        let _env = SandboxEnvGuard::set("");
        let env_opt_out = crate::harness::sandbox::opt_out_from_env();
        assert_eq!(
            env_opt_out,
            crate::harness::pty::OptOut::None,
            "an empty env value is not an opt-out"
        );
```

The two sibling tests that mutate the **same** key, `src/config.rs:921-927`,
`src/config.rs:960-966`, `src/config.rs:986-988`, `src/config.rs:996-998`:

```rust
    fn sandbox_env_opt_out_wins_independently_of_the_knob() {
        let _guard = ActiveConfigTestGuard::install(Config {
            sandbox_disabled: false,
            ..Config::default()
        });
        let _env = SandboxEnvGuard::set("1");
...
    fn sandbox_knob_opts_out_without_the_env_var() {
        let _guard = ActiveConfigTestGuard::install(Config {
            sandbox_disabled: true,
            ..Config::default()
        });
        let _env = SandboxEnvGuard::set("0"); // present but falsy: not an env opt-out
...
        // Both sources at once: still a Skip, attributed to the env variable.
        let _both = SandboxEnvGuard::set("yes");
...
        let _off = ActiveConfigTestGuard::install(Config::default());
        let _none = SandboxEnvGuard::set("");
```

How the variable is read in production/tests, `src/harness/sandbox.rs:188-205`:

```rust
    /// The operator's explicit opt-out, read from the environment with t-034b's
    /// fail-closed parsing. Call sites must use this instead of re-reading the variable.
    #[must_use]
    pub fn opt_out_from_env() -> crate::harness::pty::OptOut {
        opt_out_from_value(std::env::var(SANDBOX_OPTOUT_ENV).ok().as_deref())
    }

    /// Pure half of [`opt_out_from_env`]: only an unambiguous truthy value opts out;
    /// unset, empty, malformed or falsy-looking values keep the sandbox on.
    #[must_use]
    pub fn opt_out_from_value(raw: Option<&str>) -> crate::harness::pty::OptOut {
        match raw {
            Some(value) if is_optout_value(value) => crate::harness::pty::OptOut::Explicit {
                setting: format!("{SANDBOX_OPTOUT_ENV}={value}"),
            },
            _ => crate::harness::pty::OptOut::None,
        }
    }
```

(the harness has a second, private reader — `src/harness/pty.rs:648-657`,
`fn opt_out_from_env()` — called from `sandbox_decision_for_current_process()`,
`src/harness/pty.rs:712`; the key itself is `src/harness/pty.rs:478`,
`pub const SANDBOX_OPTOUT_ENV: &str = "MARMEL_DISABLE_SANDBOX";`, truthy set at
`src/harness/pty.rs:481`.)

### Why it fails on Windows

**The value under test is process-global state read at read-time, and three tests in
the same lib test binary write it concurrently.** That is the real cause, not a
Windows-only semantic (Windows makes it *likely*, and adds one genuine semantic trap):

1. **The asserted precondition is not owned by the test.** `opt_out_from_env()`
   (`sandbox.rs:192`) reads the *process environment at the moment of the call*. The
   only way the test influences it is by mutating that same global through
   `SandboxEnvGuard::set`.
2. **The guard's own SAFETY comment is false for CI.** It claims "these tests are
   gated with `--test-threads=1`" (`src/config.rs:790-792`). CI runs
   `cargo test --all-targets --all-features --verbose` with **no** `--test-threads`
   (`.github/workflows/ci.yml:72`); `--test-threads=1` occurs nowhere in `.github/`.
   So `config::tests::sandbox_disabled_knob_reaches_sandbox_inputs_opt_out` (:885),
   `sandbox_env_opt_out_wins_independently_of_the_knob` (:922) and
   `sandbox_knob_opts_out_without_the_env_var` (:961) run **concurrently**, writing
   `""`, `"1"`, `"0"`, `"yes"` to the same key and restoring/removing it from
   different threads (last writer wins).
3. **Measured on this box (same code path CI executes):**

   ```
   ./target/debug/deps/marmennill-25e1737d17c27a1e 'config::tests::sandbox'                     → 33/40 runs FAILED  (default = 56 threads)
   ... same filter --test-threads=4   (windows-latest vCPU count)                                →  6/12 runs FAILED
   ... same filter --test-threads=1                                                             →  0/5  runs FAILED
   ```

   Observed panics (all order-dependent):

   ```
   src/config.rs:930:9  left: None,  right: Explicit { setting: "MARMEL_DISABLE_SANDBOX=1" }   ← env clobbered between set and read
   src/config.rs:989:9  left: Explicit { "sandbox_disabled=true" }, right: Explicit { "MARMEL_DISABLE_SANDBOX=yes" }
   src/config.rs:900:9  left: None,  right: Explicit { setting: "sandbox_disabled=true" }       ← ACTIVE_CONFIG race
   src/config.rs:969:9 / 972:9 / 999:9                                                          ← same two races
   ```

   Note the `:900/:972/:999` family is the **second** process global mutated by the
   same tests: `ActiveConfigTestGuard::install` (`src/config.rs:760-779`) writes the
   global `ACTIVE_CONFIG: RwLock<Option<Config>>` (`src/config.rs:195-212`), which
   `sandbox_opt_out` → `sandbox_disabled()` (`src/config.rs:272-274, 301-303`) reads
   back. A concurrent `ActiveConfigTestGuard::install(Config::default())` (e.g.
   `src/config.rs:997`) makes the knob-derived expectation at `:900`/`:972` fail. Any
   fix must close **both** globals, or the site-1 failure survives.
4. **Windows aggravators / genuine Windows semantics:**
   * **`set_var(key, "")` cannot express "present but empty" on Windows.** The Win32
     contract of `SetEnvironmentVariableW` is that an empty value deletes the
     variable (std's Windows `set_var` follows it: empty value → delete path). So
     `SandboxEnvGuard::set("")` at `:891` and `:998` on Windows actually means
     *unset*, and the assertion at `:893-897` ("an empty env value is not an
     opt-out") silently tests the unset case — the fail-closed empty-value contract
     is untested on Windows even when the test "passes".
   * **Leaked ambient values:** because the `Drop` restore is last-writer-wins across
     racing guards, `"1"`/`"yes"` can be left in the process environment for the rest
     of the binary; every later reader (`sandbox.rs:192`, `pty.rs:650`, and in
     production `main.rs:83`) inherits it. That is exactly the "ambient value makes
     the assertion wrong" case.
   * **Rust 2024 makes `set_var`/`remove_var` `unsafe`** (hence the `unsafe` blocks at
     `src/config.rs:793` and `:804`; `edition = "2024"` in `Cargo.toml:4`). The
     precondition for that safety is "no other thread reads the env while it is
     mutated" — violated here by construction, since the sibling tests and
     `sandbox_decision_for_current_process()` read the same key.
   * **No safe env wrapper exists in the crate to reuse.** `grep` for
     `set_var|remove_var` over `src` + `tests` yields hits **only** in
     `src/config.rs:793,804,807,810`. `src/harness/pty_tests.rs` does **not** touch
     the process env at all; the crate's established, portable pattern is (a) feed the
     **pure** parser with an explicit value — `OptOut::Explicit { setting: … }` in
     `src/harness/pty_tests.rs:949,959` and `src/harness/sandbox.rs:708-741` — and
     (b) inject env into the **child** via `CommandBuilder` —
     `.env(SANDBOX_OPTOUT_ENV, "1")` at `src/harness/sandbox.rs:982,1048,1128` and
     `.env_remove(SANDBOX_OPTOUT_ENV)` at `src/harness/sandbox.rs:1086,1160,1210`.
     The fix must reuse those patterns, not invent a new one.

### Minimal portable fix

**Step 1 — remove process-env mutation from the three *decision* tests; drive them
through the existing pure seam `crate::harness::sandbox::opt_out_from_value`
(`src/harness/sandbox.rs:198-205`).** No `unsafe`, no ordering dependency, and the
"present but empty" case becomes expressible on every platform:

```rust
        let _guard = ActiveConfigTestGuard::install(cfg);
        // Env unset: the env parser contributes `None`, so only the knob can opt out.
        // The pure parser is driven directly: mutating the process env here would be
        // a data race with the sibling tests in this binary (and on Windows
        // `set_var(key, "")` cannot even express "present but empty").
        let env_opt_out = crate::harness::sandbox::opt_out_from_value(Some(""));
        assert_eq!(
            env_opt_out,
            crate::harness::pty::OptOut::None,
            "an empty env value is not an opt-out"
        );
```

same substitution at `:929` → `opt_out_from_value(Some("1"))`, `:968` →
`opt_out_from_value(Some("0"))`, `:988` → `opt_out_from_value(Some("yes"))`,
`:1000` → `opt_out_from_value(None)`, deleting the corresponding
`SandboxEnvGuard::set(...)` lines. (`opt_out_from_value` is `pub` in the `pub mod
sandbox` declared at `src/harness/mod.rs:22`, and it compiles on non-Linux: the whole
non-Linux half of `sandbox.rs` is live per `docs/sandbox_deadcode_audit.md:116`.)

**Step 2 — keep exactly one test that proves the env is *actually read*, and make it
serialized with an explicit set/remove (the only place `set_var` stays `unsafe`):**

```rust
    /// One lock for every test in this binary that touches a process-global
    /// (the `MARMEL_DISABLE_SANDBOX` key or `ACTIVE_CONFIG`). Rust 2024 makes
    /// `set_var`/`remove_var` unsafe exactly because an unsynchronized read on
    /// another thread races with them — CI never passes `--test-threads=1`
    /// (`.github/workflows/ci.yml:72`), so this lock is the only gate.
    static TEST_SERIAL: std::sync::Mutex<()> = const { std::sync::Mutex::new(()) };

    fn lock_test_serial() -> std::sync::MutexGuard<'static, ()> {
        // Poisoning is ignored on purpose: a failing assertion must not cascade
        // into every later test of this module.
        TEST_SERIAL.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    struct SandboxEnvGuard {
        _serial: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl SandboxEnvGuard {
        /// Set the key to a non-empty value for one test.
        /// The empty case is asserted through `sandbox::opt_out_from_value(Some(""))`:
        /// on Windows an empty value is indistinguishable from unset
        /// (`SetEnvironmentVariableW` deletes the variable), so this helper forbids it.
        fn set(value: &str) -> Self {
            assert!(
                !value.is_empty(),
                "empty is not representable on Windows — use opt_out_from_value(Some(\"\"))"
            );
            let serial = lock_test_serial();
            let previous = std::env::var_os(crate::harness::pty::SANDBOX_OPTOUT_ENV);
            // SAFETY: `TEST_SERIAL` is held for the whole test, every test in this
            // binary that reads or writes this key holds the same lock, and the
            // captured value is restored on drop.
            unsafe {
                std::env::set_var(crate::harness::pty::SANDBOX_OPTOUT_ENV, value);
            }
            Self {
                _serial: serial,
                previous,
            }
        }

        /// Explicitly remove the key (the "env absent" case), for one test.
        fn clear() -> Self {
            let serial = lock_test_serial();
            let previous = std::env::var_os(crate::harness::pty::SANDBOX_OPTOUT_ENV);
            // SAFETY: as above.
            unsafe {
                std::env::remove_var(crate::harness::pty::SANDBOX_OPTOUT_ENV);
            }
            Self {
                _serial: serial,
                previous,
            }
        }
    }

    impl Drop for SandboxEnvGuard {
        fn drop(&mut self) {
            // SAFETY: `TEST_SERIAL` is still held (dropped after this guard).
            unsafe {
                match self.previous.take() {
                    Some(previous) => {
                        std::env::set_var(crate::harness::pty::SANDBOX_OPTOUT_ENV, previous);
                    }
                    None => {
                        std::env::remove_var(crate::harness::pty::SANDBOX_OPTOUT_ENV);
                    }
                }
            }
        }
    }
```

**Step 3 — take the same lock in the config-global guards** so the `:900/:972/:999`
failures are closed too: `ActiveConfigTestGuard::install`
(`src/config.rs:766-770`) acquires `lock_test_serial()` and stores the guard in the
struct, and the inline save/clear/restore blocks at `src/config.rs:852-857`
(`sandbox_disabled_knob_defaults_to_false`) and `src/config.rs:1036-1045`
(`command_timeout_secs_default_when_no_config_is_active`) wrap their critical section
in the same lock.

**Step 4 — correct the two SAFETY comments** at `src/config.rs:790-792` and
`:802-803`: they must state the real invariant ("`TEST_SERIAL` held"), not the
`--test-threads=1` claim that CI never satisfies.

No new dependencies are needed: `Cargo.toml` dev-deps are only `tempfile` and
`wiremock`, and `std::sync::Mutex` + `const { ... }` covers serialization.

### Risks / interactions with other tests

* **Serialization scope.** A `config`-module-local mutex orders the config tests only.
  `ACTIVE_CONFIG` is also mutated by `src/orchestrator/notice_tests.rs:403,508,607`,
  `src/harness/dispatch_integrity_tests.rs:40-58` (its own `ActiveConfigGuard`),
  `src/config.rs:698`, and `tests/test_ui_session.rs:755` (different binary). If CI
  still shows config/notice flakiness, promote the mutex to one in-crate test helper
  (e.g. `#[cfg(test)] pub fn config::test_serial()`) and use it in all of them.
* **Do not "fix" this with `--test-threads=1` in CI.** It masks the race, costs
  runtime (a serial full-lib run is ~84 s per `docs/baseline_test_run.md:34`), and the
  docs already use serial mode only as a diagnostic floor
  (`docs/baseline_test_run.md:321`, `docs/verification_final.md:44`).
* **Do not delete env coverage.** After Step 1, the env→decision wiring is still
  covered twice: the retained Step-2 test (`opt_out_from_env()`), and the
  child-process end-to-end tests in `src/harness/sandbox.rs:982,1048,1064,1086,1107,1128,1160,1210`
  (including the falsy `=false` fail-closed case at `:1128`).
* **Leaked env affects other tests' log lines.** A leftover
  `MARMEL_DISABLE_SANDBOX=1` changes the *reason* logged by
  `sandbox_decision_for_current_process()` (`src/harness/pty.rs:712-715`); in a test
  binary the argv is unchanged (already `Skip { TestHarnessExe }`, `pty.rs:617-621`),
  so Site 2's argv assertions stay valid — but log-text assertions
  (`src/harness/pty_tests.rs:966`, `:1101`) are reason-sensitive and must not be
  tightened against a specific reason string.
* **Mutex poisoning** is deliberately swallowed; otherwise one panicking assertion
  would fail every later config test in the same binary.

---

## Site 2 — `src/harness/pty_tests.rs:1098`, `assert_eq!(argv[0], "sh")`

### Current code

`src/harness/pty_tests.rs:1085-1111`:

```rust
/// End-to-end wiring at the decision point, PTY-free: in a test-harness binary
/// the builder skips (and warns) instead of exec-ing a binary that cannot
/// re-enter, and the t-034a `stty`/`ulimit` preamble survives either way.
#[test]
fn test_build_sandboxed_command_logs_its_choice_and_keeps_the_wrapper() {
    let (built, logs) = capture_logs(|| build_sandboxed_command("echo hi", Path::new("/tmp")));
    let cmd = built.expect("argv must be buildable");
    let argv = cmd.get_argv();
    assert!(!argv.is_empty(), "the builder must have a program");

    // In a test binary the re-entry is skipped, so this is `sh -c <wrapped>`;
    // either way the command was chosen by a decision that was logged.
    if cfg!(test) {
        assert_eq!(argv[0].to_string_lossy(), "sh");
        assert_eq!(argv[1].to_string_lossy(), "-c");
        assert!(
            logs.contains("WARN") && logs.contains("pty sandbox decision"),
            "every skip must leave a visible trace, got {logs:?}"
        );
    }
    let wrapped = argv.last().expect("the wrapped command").to_string_lossy();
    assert!(
        wrapped.starts_with("stty -echo 2>/dev/null || true; ulimit -f 4194304"),
        "REQ-TOOL-001 preamble must survive the sandbox rewrite, got {wrapped:?}"
    );
    assert!(wrapped.ends_with("echo hi"));
}
```

What produces `argv` — `src/harness/pty.rs:756-794` (the PTY spawn argument builder):

```rust
pub fn build_sandboxed_command(
    command: &str,
    cwd: &std::path::Path,
) -> Result<CommandBuilder, ToolError> {
    if cfg!(target_os = "windows") {
        let trimmed = command.trim();
        tracing::warn!(
            resolved_exe = %resolved_exe_path()
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<unresolved>".to_string()),
            mode = "unsandboxed",
            reason = "platform has no Landlock",
            "pty sandbox SKIPPED: Windows has no Landlock, this command runs unsandboxed"
        );
        if trimmed.eq_ignore_ascii_case("cmd")
            || trimmed.eq_ignore_ascii_case("cmd.exe")
            || trimmed.eq_ignore_ascii_case("powershell")
            || trimmed.eq_ignore_ascii_case("powershell.exe")
        {
            let mut cmd = CommandBuilder::new(trimmed);
            cmd.cwd(cwd);
            Ok(cmd)
        } else {
            let mut cmd = CommandBuilder::new("cmd");
            cmd.args(["/C", command]);
            cmd.cwd(cwd);
            Ok(cmd)
        }
    } else {
        let (program, args) =
            sandbox_command_argv(&sandbox_decision_for_current_process(), command, cwd)?;
        let mut cmd = CommandBuilder::new(program);
        for arg in args {
            cmd.arg(arg);
        }
        cmd.cwd(cwd);
        Ok(cmd)
    }
}
```

and the non-Windows argv, `src/harness/pty.rs:725-747` (relevant arms):

```rust
    let wrapped = format!(
        "stty -echo 2>/dev/null || true; ulimit -f {ULIMIT_FILE_BLOCKS} 2>/dev/null || ulimit -f {ULIMIT_FILE_BLOCKS_FALLBACK} 2>/dev/null; {command}"
    );
    match decision {
        SandboxDecision::Apply { resolved_exe } => Ok((
            resolved_exe.to_string_lossy().into_owned(),
            vec![
                SANDBOX_EXEC_ARG.to_string(),
                cwd.to_string_lossy().into_owned(),
                wrapped,
            ],
        )),
        SandboxDecision::Skip { .. } => Ok(("sh".to_string(), vec!["-c".to_string(), wrapped])),
```

`CommandBuilder::get_argv` returns the stored argv verbatim — vendored
`portable-pty-0.9.0/src/cmdbuilder.rs:290-292` (`pub fn get_argv(&self) ->
&Vec<OsString> { &self.args }`) and `:215` (`CommandBuilder::new(program)` seeds
`args` with exactly that token). portable-pty never rewrites `argv[0]`.

### Why it fails on Windows

For `build_sandboxed_command("echo hi", "/tmp")` the two platforms build **different
argv shapes and emit different log lines**:

| | Unix (`cfg!(test)` → `Skip { TestHarnessExe }`) | Windows (`cfg!(target_os = "windows")` early branch) |
|---|---|---|
| `argv[0]` | `"sh"` (pty.rs:742) | **`"cmd"`** (pty.rs:779) — bare token, **not** `cmd.exe`, **not** `/C` |
| `argv[1]` | `"-c"` (pty.rs:742) | **`"/C"`** (pty.rs:780) |
| `argv.len()` | 3 | 3 |
| `argv.last()` | `stty -echo 2>/dev/null \|\| true; ulimit -f 4194304 2>/dev/null \|\| ulimit -f 2097152 2>/dev/null; echo hi` | **`"echo hi"`** — the REQ-TOOL-001 preamble is *not applied* on Windows (`stty`/`ulimit` are POSIX shell builtins) |
| logs | `DEBUG … "pty sandbox decision"` (pty.rs:674) **and** `WARN … "pty sandbox SKIPPED: …"` (pty.rs:690) | **only** the `WARN … "pty sandbox SKIPPED: Windows has no Landlock, this command runs unsandboxed"` (pty.rs:762-769); the `debug!` line `"pty sandbox decision"` comes from `log_sandbox_decision`, which the Windows branch never calls |

So on Windows **four** assertions in one test fail: `argv[0]` (`:1098`), `argv[1]`
(`:1099`), the preamble `starts_with` (`:1106-1109`) and `logs.contains("pty sandbox
decision")` (`:1101`). The `if cfg!(test)` guard at `:1097` is always true inside a
test binary and therefore does **not** protect them.

(`argv[0]` being `"cmd"` and not `"cmd.exe"` is a fact of the production call site:
`CommandBuilder::new("cmd")` at `pty.rs:779`. The OS resolves `cmd` →
`%SystemRoot%\System32\cmd.exe` at spawn time; `get_argv()` still reports `cmd`.)

### Minimal portable fix

Use the crate's existing *platform spawn-spec* idiom
(`src/harness/pty_tests.rs:335-355`, `short_lived_child_spec()` /
`long_running_child_spec()` — `("cmd", &["/C", "exit 0"])` vs `("true", &[])`) and
make every expectation in this test platform-conditional:

```rust
/// The expected argv of `build_sandboxed_command` on this platform. Grounded in
/// the production builder: `cmd /C <command>` on Windows (`pty.rs:779-780`,
/// bare `cmd` token — portable-pty keeps argv0 verbatim) and `sh -c <wrapped>`
/// for every skip on unix (`pty.rs:742`).
fn expected_sandbox_shell() -> (&'static str, &'static str) {
    if cfg!(windows) {
        ("cmd", "/C")
    } else {
        ("sh", "-c")
    }
}

#[test]
fn test_build_sandboxed_command_logs_its_choice_and_keeps_the_wrapper() {
    let (built, logs) = capture_logs(|| build_sandboxed_command("echo hi", Path::new("/tmp")));
    let cmd = built.expect("argv must be buildable");
    let argv = cmd.get_argv();
    assert!(!argv.is_empty(), "the builder must have a program");

    let (program, switch) = expected_sandbox_shell();
    if cfg!(test) {
        assert_eq!(argv[0].to_string_lossy(), program);
        assert_eq!(argv[1].to_string_lossy(), switch);
        // Both platforms warn; only the shared "SKIPPED" wording is asserted,
        // because the Windows branch warns from `build_sandboxed_command` itself
        // (`pty.rs:768`) and never emits the unix `debug!("pty sandbox decision")`.
        assert!(
            logs.contains("WARN") && logs.contains("SKIPPED"),
            "every skip must leave a visible trace, got {logs:?}"
        );
    }

    let last = argv.last().expect("the wrapped command").to_string_lossy();
    if cfg!(windows) {
        // No Landlock on Windows, and `stty`/`ulimit` are POSIX shell builtins:
        // the command is handed to `cmd /C` verbatim.
        assert_eq!(last, "echo hi", "the Windows builder must not rewrite the command");
    } else {
        assert!(
            last.starts_with("stty -echo 2>/dev/null || true; ulimit -f 4194304"),
            "REQ-TOOL-001 preamble must survive the sandbox rewrite, got {last:?}"
        );
        assert!(last.ends_with("echo hi"));
    }
}
```

Notes for the implementer:

* Keep `to_string_lossy()` (argv is `Vec<OsString>`); never compare `OsString` to a
  `&str` directly.
* The platform-neutral log assertion is `logs.contains("WARN") &&
  logs.contains("SKIPPED")`, which is exactly the form already used at
  `src/harness/pty_tests.rs:966`.
* Optional follow-up (production-side, **not** required to fix the test): route the
  Windows branch through the same `should_apply_sandbox` + `log_sandbox_decision`
  path so one log format serves both platforms (`SandboxSkip::UnsupportedPlatform`
  already exists, `pty.rs:521` / `pty.rs:607-613`). Out of scope here: it changes
  production log text asserted at `pty_tests.rs:1010-1028` and `:966`.

### Risks / interactions with other tests

* **Do not assert `cmd.exe`** — nothing in the crate ever passes `cmd.exe` to the
  builder for this path (`pty.rs:779` uses `"cmd"`); only the *bare-command*
  special-case at `pty.rs:770-777` can produce `cmd.exe`/`powershell`, and that
  branch is untested today (`argv.len() == 1`, no `/C`). Worth adding as a separate
  case, not folded into this test.
* **`wrapped.starts_with("stty …")` has no Windows equivalent.** If the team later
  wants preamble parity on Windows, that is a production change to
  `build_sandboxed_command` (and a policy question — `stty`/`ulimit` do not exist
  there); until then the `cfg!(windows)` split above is the honest assertion.
* **Sibling PTY tests already use `cfg(windows)`/`cfg(unix)`** for spawn specs and
  zombie checks (`src/harness/pty_tests.rs:95-96,112-115,123-126,336,348,373,386`) —
  matching that style avoids "new pattern" churn and keeps `cargo clippy
  --all-targets -D warnings` (CI `lint` job, ci.yml:38) clean, e.g. no
  dead-code/unused-import warnings on the Windows half.
* 4 `harness::pty::tests::*` failures are already **sanctioned on Linux** for an
  unrelated reason (`openpty` EACCES, `docs/baseline_test_run.md:160-166`), so use the
  test *names*, not counts, when verifying this fix.

---

## Site 3 — `src/harness/search_tests.rs:430` and `:699` (glob brace + wildcard matching)

### Current code

`src/harness/search_tests.rs:428-441` (site A, brace pattern):

```rust
    // Through the real tool, exactly the shape from the bug report.
    let via_tool = glob_at(&dir, serde_json::json!({"pattern": "src/**/*.{rs,md}"})).unwrap();
    assert_eq!(
        via_tool
            .content
            .lines()
            .map(normalize_for_compare)
            .collect::<Vec<_>>(),
        vec!["src/a.md", "src/a.rs", "src/deep/b.md", "src/deep/b.rs"],
        "the brace pattern must match both extensions"
    );

    let outcome = glob_in_root("**/*.{rs,md}", &dir, GLOB_HARD_CAP).unwrap();
```

(continues at `:442-453` with `normalize_paths(&outcome.matches)` vs
`["src/a.md","src/a.rs","src/deep/b.md","src/deep/b.rs","top.md","top.rs"]`, and
`:454-457` `total_matches == 6`.)

`src/harness/search_tests.rs:699-706` (site B, `**`/`?` translation):

```rust
    assert_eq!(
        normalize_paths(
            &glob_in_root("**/odd.rs", &dir, GLOB_HARD_CAP)
                .unwrap()
                .matches
        ),
        vec!["nested/dir/odd.rs"]
    );
    assert_eq!(
        normalize_paths(
            &glob_in_root("nested/a?.rs", &dir, GLOB_HARD_CAP)
                .unwrap()
                .matches
        ),
        vec!["nested/a1.rs"]
    );
```

The test-side helpers, `src/harness/search_tests.rs:41-72` (abridged to the two
functions named in the task):

```rust
/// Canonical spelling of a path-ish string for comparison: drops a Windows
/// verbatim/device prefix and renders `\` as `/`.
///
/// **No-op on Unix**: under `cfg!(unix)` the input is returned untouched, byte
/// for byte, so a Unix run compares exactly the strings it always compared.
fn normalize_for_compare(raw: &str) -> String {
    if cfg!(unix) {
        return raw.to_owned();
    }
    strip_windows_path_prefix(raw).replace('\\', "/")
}
...
/// `matches`-list flavour of [`normalize_for_compare`]: element-by-element, so
/// the expected list keeps asserting the exact same set in the exact same order.
fn normalize_paths(paths: &[String]) -> Vec<String> {
    paths
        .iter()
        .map(|path| normalize_for_compare(path))
        .collect()
}
```

The production collection step, `src/harness/search.rs:281-301`:

```rust
    let mut matched: BTreeSet<String> = BTreeSet::new();
    let mut total_matches: usize = 0;
    for entry in search_walker(root).build().flatten() {
        let p = entry.path();
        if !p.is_file() {
            continue;
        }
        let rel = p
            .strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .to_string();
        let rel = rel.trim_start_matches("./").to_string();
        if regexes.iter().any(|re| re.is_match(&rel)) {
            total_matches += 1;
            matched.insert(rel);
            if matched.len() > max_results {
                matched.pop_last();
            }
        }
    }
```

There is **no** `replace('\\', "/")` anywhere in `src/harness/search.rs` (grep:
separator handling exists only in `glob_to_regex` and in the test helpers).

The matcher, `src/harness/search.rs:497-532`:

```rust
/// Translate a glob pattern into an anchored regex.
fn glob_to_regex(pattern: &str) -> Result<Regex, String> {
    let mut re = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // `\X` matches the literal character X (so `\` matches a brace).
                match chars.next() {
                    Some(escaped) => re.push_str(&regex::escape(&escaped.to_string())),
                    None => re.push_str("\\\\"),
                }
            }
            '*' => {
                if chars.peek() == Some(&'*') {
                    chars.next();
                    // `**` matches across path separators.
                    if chars.peek() == Some(&'/') {
                        chars.next();
                        re.push_str("(?:.*/)?");
                    } else {
                        re.push_str(".*");
                    }
                } else {
                    re.push_str("[^/]*");
                }
            }
            '?' => re.push_str("[^/]"),
            '.' => re.push_str("\\."),
            '/' => re.push('/'),
            other => re.push_str(&regex::escape(&other.to_string())),
        }
    }
    re.push('$');
    Regex::new(&re).map_err(|e| format!("pattern compiles to an invalid matcher: {e}"))
}
```

(the doc comment line quoted above is `// \X matches the literal character X (so
`\{` matches a brace).` in the source, `search.rs:504`.)

The existing platform-helper test to extend, `src/harness/search_tests.rs:728-761`:

```rust
/// The comparison helper itself: a byte-identical passthrough on Unix, and a
/// verbatim-prefix + separator canonicalisation on Windows. Each half is
/// asserted under its own platform, so the guard can never weaken the other.
#[test]
fn normalize_for_compare_is_identity_on_unix_and_canonical_on_windows() {
    if cfg!(unix) {
        ...
    }
    if cfg!(windows) {
        assert_eq!(
            normalize_for_compare(r"\\?\C:\Users\tmp\adir\first.txt"),
            "C:/Users/tmp/adir/first.txt"
        );
        assert_eq!(normalize_for_compare(r"\\.\C:\tmp\x"), "C:/tmp/x");
        assert_eq!(
            normalize_for_compare(r"\\localhost\C$\tmp\a.md"),
            "C$/tmp/a.md"
        );
        assert_eq!(normalize_for_compare(r"C:\tmp\src\a.md"), "C:/tmp/src/a.md");
    }
    // Root-relative entries keep the exact literals the seeded tree asserts.
    assert_eq!(normalize_for_compare("adir/first.txt"), "adir/first.txt");
    assert_eq!(normalize_for_compare("{literal}.rs"), "{literal}.rs");
}
```

### Why it fails on Windows

**The glob grammar is `/`-based, but the collected path spelling is native
(`\` on Windows), so the match step yields *zero* matches — and normalizing only the
output side cannot help, because there is nothing to normalize.**

* `glob_to_regex` translates a pattern's `/` into a **literal `/`** (`search.rs:526`),
  `*` into `[^/]*` (`:521`), `?` into `[^/]` (`:524`) and `**/` into `(?:.*/)?`
  (`:514-516`).
* `glob_in_root` builds `rel` from `strip_prefix(root).to_string_lossy()`
  (`:288-292`) — on Windows that spells nested paths with `\` (and `dir.join("src/deep")`
  in the fixture even yields *mixed* `src/deep\b.rs`).
* Replay of the emitted regexes against the exact Windows spellings:

  ```
  pattern `src/**/*.{rs,md}` → alt regex ^src/(?:.*/)?[^/]*\.rs$
      subject "src\a.md"        match=False      subject "src/deep/b.md"  match=True (wrong spelling only)
  pattern `**/odd.rs`        → ^(?:.*/)?odd\.rs$
      subject "nested\dir\odd.rs"  match=False   subject "nested/dir/odd.rs"  match=True
  pattern `nested/a?.rs`     → ^nested/a[^/]\.rs$
      subject "nested\a1.rs"  match=False        subject "nested/a1.rs"   match=True
  ```

* Consequences at the two cited lines:
  * `:430` — the tool returns the *no-match* payload `"no matches"`
    (`src/harness/search.rs:224-226`), so the compared vector is `["no matches"]`
    vs 4 expected paths; the follow-up block at `:442-457` compares `[]` vs 6 paths
    and `total_matches: 0` vs `6`.
  * `:699-706` — `[]` vs `["nested/dir/odd.rs"]`, `[]` vs `["nested/a1.rs"]`. (The
    escaped-brace case at `:715-723` *passes*, because `{literal}.rs` has no
    separator.)
* This is **a production bug, not a test bug**: on Windows any `glob` call with a
  directory component (`src/**/*.rs`, `docs/**/*.{md,txt}`) silently returns "no
  matches" to the agent. The `normalize_for_compare`/`normalize_paths` helpers only
  canonicalise strings that were actually produced; they cannot manufacture matches.

### Minimal portable fix

**Both sides, one place — normalize the collected path to `/` *before matching and
before insertion into the ordered set* (`src/harness/search.rs:288-293`):**

```rust
        let rel = p
            .strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .to_string();
        // The glob grammar (`glob_to_regex`) is `/`-based: `/` → literal `/`,
        // `*` → `[^/]*`, `?` → `[^/]`, `**/` → `(?:.*/)?`. The walker spells
        // separators natively (`\` on Windows, possibly mixed when the caller
        // passed `/` inside a component), so canonicalise **once**, before
        // matching and before the ordered set: matching, the reported list and
        // the "lexicographically first N" cap then all agree with the documented
        // root-relative contract on every platform.
        // Gated on Windows so a Unix file name that legitimately contains `\`
        // is never rewritten.
        #[cfg(windows)]
        let rel = rel.replace('\\', "/");
        let rel = rel.trim_start_matches("./").to_string();
```

That single change fixes `search_tests.rs:430` and `:699` **and** the sibling
assertions that are broken by the same defect (`:442-457`, and the truncation
contract in `grep_and_glob_truncation_is_deterministic_across_runs`,
`src/harness/search_tests.rs:240`, with its `adir/first.txt` ordering at
`:269-291` and `total_matches: 32` / `:14` at `:271`/`:306`).

**Rejected alternative:** loosening the matcher to `[/\\]` (`'/' => re.push_str("[/\\\\]")`)
— it would keep Windows reporting `\`-spelled matches, contradicting the module
contract "Matches are relative paths" (`src/harness/search.rs:212-216`), leave
`[^/]`/`.*` classes inconsistent with the pattern language, and force
test-side normalization forever.

**Keep the test helpers** (`normalize_for_compare` / `normalize_paths`) as
defence-in-depth for the *grep* side, whose lines carry the absolute walker path
(`src/harness/search.rs:143`: `results.push(format!("{}:{}: {}", path.display(), idx + 1, line))`),
where the verbatim-prefix trimming is still meaningful.

**Extend the named platform test** `normalize_for_compare_is_identity_on_unix_and_canonical_on_windows`
(`src/harness/search_tests.rs:732`, ends at `:761`) with a **production-side
regression assertion**, appended after `:760` so it runs on *every* platform:

```rust
    // Production-side regression: the tool's own output must already be
    // `/`-spelled and root-relative, so this comparison helper is never
    // load-bearing for a passing glob run (it stays load-bearing for grep's
    // absolute paths only).
    let dir = temp_dir();
    fs::create_dir_all(dir.join("nested/dir")).unwrap();
    fs::write(dir.join("nested/dir/odd.rs"), "x\n").unwrap();
    let outcome = glob_in_root("**/odd.rs", &dir, GLOB_HARD_CAP).expect("pattern compiles");
    assert_eq!(
        outcome.matches,
        vec!["nested/dir/odd.rs"],
        "glob_in_root must match and report `/`-separated paths on every platform"
    );
    assert_eq!(
        normalize_paths(&outcome.matches),
        outcome.matches,
        "the comparison helper must be a no-op on real production output"
    );
    fs::remove_dir_all(&dir).unwrap();
```

### Risks / interactions with other tests

* **Unix output stays byte-identical** (the replacement is `#[cfg(windows)]`-gated),
  so the 800+ existing lib assertions in `search_tests.rs` are untouched on Linux.
* **Windows ordering/cap semantics change deliberately.** `matched` is a
  `BTreeSet<String>` and the cap keeps the "lexicographically first" entries
  (`src/harness/search.rs:278-299` + `GLOB_TRUNCATIONFooter` wording at `:333`).
  After normalization the comparison uses `/` (0x2F) instead of `\` (0x5C), which can
  flip survivors in trees mixing a separator with, e.g., an uppercase sibling
  (`src/A` vs `srcA/b`: `/` < `A` but `\` > `A`). This *aligns Windows with the Unix
  contract those tests already assert* — but do not be surprised that the Windows
  capped list changes.
* **Fixes more than the two cited failures** (see list above): expected, and they are
  the same defect, not regressions.
* `expand_braces` unit tests (`src/harness/search_tests.rs:520-524`) and the
  separator-free patterns (`"*.rs"` `:153`, `"*.toml"` `:468`, `"*.{rs}"` `:597`) are
  unaffected — they match today and after the change.
* Tool-visible behaviour on Windows changes: `matches` are joined with `\n` into the
  tool payload (`src/harness/search.rs:228`), so agents will now see `/`-paths
  consistent with the documented grammar (`src/harness/search.rs:43-54`). No test
  asserts `\`-separated glob output (grep over `src/harness/*tests*`: the only
  backslash expectations are the `WINDOWS_PATH_PREFIXES` fixtures and the
  `expand_braces` escape cases).
* Interaction with Site 1/4 style global state: none — these tests are tempdir-scoped
  (`temp_dir()` at `src/harness/search_tests.rs:77-88`) and workspace-scoped via
  `with_workspace_root` (`:92-100`).

---

## Site 4 — `src/mcp/http_tests.rs:462` (source-text scan of `client.rs` / `http.rs`)

### Current code

`src/mcp/http_tests.rs:440-480` (the whole test, verbatim):

```rust
    /// Cluster C7: the transport must not own a second copy of the protocol.
    /// Only the *production* half of each transport file is inspected — the test
    /// modules deliberately quote wire literals to pin them.
    #[test]
    fn mcp_http_transport_keeps_no_protocol_of_its_own() {
        fn production_only(src: &str) -> &str {
            src.split("#[cfg(test)]\nmod tests {").next().unwrap_or(src)
        }
        for src in [
            production_only(include_str!("http.rs")),
            production_only(include_str!("client.rs")),
        ] {
            assert!(
                !src.contains("jsonrpc:\"2.0\"") && !src.contains("jsonrpc: \"2.0\""),
                "the transports must not build JSON-RPC envelopes themselves"
            );
            for method in [
                METHOD_INITIALIZE,
                METHOD_INITIALIZED,
                METHOD_TOOLS_LIST,
                METHOD_TOOLS_CALL,
            ] {
                assert!(
                    !src.contains(&format!("\"{method}\"")),
                    "the transports must not spell out the method name '{method}'"
                );
            }
            assert!(
                !src.contains("MCP_PROTOCOL_VERSION") && !src.contains("2024-11-05"),
                "the transports must not carry the initialize params"
            );
            assert!(
                !src.contains("AtomicU64"),
                "the transports must not own a request-id allocator"
            );
            assert!(
                !src.contains("\"inputSchema\"") && !src.contains("\"isError\""),
                "the transports must not decode results themselves"
            );
        }
    }
```

Line map: `production_only` = `:445-447`; `include_str!` pair = `:448-451`;
jsonrpc-envelope scan = `:452-455`; **method-literal loop = `:456-466` (the failing
`assert!` starts at `:462`)**; protocol-version scan = `:467-470`; `AtomicU64` =
`:471-474`; `inputSchema`/`isError` = `:475-478`.

Method literals checked (imported at `src/mcp/http_tests.rs:18-22`; defined in
`src/mcp/protocol.rs:68-74`):

```rust
pub(crate) const METHOD_INITIALIZE: &str = "initialize";
pub(crate) const METHOD_INITIALIZED: &str = "notifications/initialized";
pub(crate) const METHOD_TOOLS_LIST: &str = "tools/list";
pub(crate) const METHOD_TOOLS_CALL: &str = "tools/call";
```

So the needles at `:462` are the quoted forms `"initialize"`,
`"notifications/initialized"`, `"tools/list"`, `"tools/call"`.

The anchor target, `src/mcp/client.rs:427-428` (LF here, verified with `cat -A`):

```rust
#[cfg(test)]
mod tests {
```

and the literals that live in that inline test module (client.rs:719, :733, :741,
:757 — quoted, deliberately):

```rust
            r#"{"jsonrpc":"2.0","id":0,"method":"initialize","params":{"capabilities":{"tools":{}},"clientInfo":{"name":"marmel","version":"@VERSION@"},"protocolVersion":"2024-11-05"}}"#
...
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#
...
            r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#
```

### Why it fails on Windows

**`production_only` is an LF-anchored string split, and a `windows-latest` checkout
carries CRLF — so the anchor never matches, `unwrap_or(src)` returns the *whole*
file, and the inline test module's deliberately-quoted wire literals poison every
negative assertion.**

Replay of the exact logic over the real files (LF = Linux/macOS checkout with
`core.autocrlf=input`; CRLF = Windows runner checkout, repo has no `.gitattributes`
and the workflow sets no `core.autocrlf`):

```
src/mcp/client.rs  LF    scan=production half only (426 lines)   assert violations = []
src/mcp/client.rs  CRLF  scan=WHOLE FILE (anchor missed)         first violation = assert@:462  "initialize"
                                                                 then :462 "tools/list", :467 "2024-11-05", :475 "inputSchema"
src/mcp/http.rs    LF/CRLF  scan=WHOLE FILE (no inline test module at all)   assert violations = []
```

Notes that pin the diagnosis:

* The **first** violation under CRLF is exactly the cited line `:462` — because the
  `jsonrpc:"2.0"` scan at `:452` does *not* trip: the JSON literals spell the key as
  `"jsonrpc":"2.0"`, whereas the needle is the Rust field-init spelling
  `jsonrpc:"2.0"`. So the reported site (the method-literal loop, first iteration =
  `METHOD_INITIALIZE`) is exactly what CRLF produces.
* The scan is inherently **platform/edition/format fragile**, independent of CRLF:
  any occurrence of a quoted method name inside a comment, doc comment or string
  literal in the production half fails a *negative* text scan, and the anchor itself
  breaks under benign refactors. This repo already uses the refactor that kills the
  anchor: `#[path = "client_tests.rs"]\nmod tests;` (`src/llm/client.rs:1089-1090`,
  also `src/llm/stream.rs:868-869`, `src/llm/thinking.rs:513-514`). If
  `client.rs`'s inline tests were extracted the same way, the anchor would never
  match **on any platform** and the guard would fail-open into a whole-file scan.
  `http.rs` is already in that state — it has **no** inline test module at all
  (its tests are wired at `src/mcp/mod.rs:23` as `mod http_tests;`), so it is always
  scanned in full today.

### Minimal portable fix

**Option A — comment/string-literal-aware scan (not recommended).** CRLF-normalize and
strip comments and string literals from the production half before applying the
negative scans (a mini Rust lexer: line comments, nested `/* */`, raw strings
`r#"…"`, char/byte literals, escape handling). It keeps the "absence" semantics but
re-implements a Rust tokenizer inside a test — high maintenance, easy false
negatives (an unterminated raw string silently skips the rest of the file), and it
still cannot tell "the transport owns protocol knowledge" from "a doc comment quotes
a wire literal", which is the thing the guard actually cares about.

**Option B — byte-tolerant slice + assertions expressed against the shared protocol
seam in `src/mcp/protocol.rs` (recommended).**

Step 1 (unblocks windows-msvc CI with **zero** production change):

```rust
        fn production_only(src: &str) -> String {
            // CRLF-tolerant: on `windows-latest` the tree is checked out with
            // `\r\n` (actions/checkout default, no `.gitattributes` here), and a
            // `\n`-anchored split silently scans the WHOLE file — test module and
            // its quoted wire literals included.
            let src = src.replace("\r\n", "\n");
            // Anchor-free cut: also survives `#[path = "x_tests.rs"] mod tests;`
            // refactors (the pattern already used in `src/llm/client.rs:1089`).
            let cut = src.find("mod tests").unwrap_or(0);
            src[..cut].to_owned()
        }
        for src in [
            production_only(include_str!("http.rs")),
            production_only(include_str!("client.rs")),
        ] {
            …unchanged assertions…
        }
```

Step 2 (durable form — replace absence heuristics with the seam, keeping the
constant-derived needles so a rename in `protocol.rs` can never silently weaken the
guard):

```rust
            // Positive: the transport must go through the shared protocol layer
            // (`src/mcp/protocol.rs:423 initialize`, `:433 list_tools`,
            // `:440 call_tool`) — today client.rs:136/250/255 and http.rs:390/566/571.
            for seam in [
                "protocol::initialize(",
                "protocol::list_tools(",
                "protocol::call_tool(",
            ] {
                assert!(
                    src.contains(seam),
                    "the transport must delegate to the shared protocol layer ({seam})"
                );
            }
            // Negative: no second protocol implementation. Needles derived from the
            // shared constants, so they cannot drift from `protocol.rs`.
            assert!(
                !src.contains("AtomicU64"),
                "the transports must not own a request-id allocator"
            );
            for method in [
                METHOD_INITIALIZE,
                METHOD_INITIALIZED,
                METHOD_TOOLS_LIST,
                METHOD_TOOLS_CALL,
            ] {
                assert!(
                    !src.contains(&format!("\"{method}\"")),
                    "the transports must not spell out the method name '{method}'"
                );
            }
```

**Recommendation: Option B** (Step 1 now, Step 2 as the durable shape).
Reasons: (i) it fixes the actual Windows failure with a one-line change and no
production behaviour change; (ii) it removes the *class* of fragility (CRLF, comment
or doc-comment quoting, test-module layout) instead of writing a Rust lexer in a
test; (iii) the invariant being protected — "the transports must not own a second
copy of the protocol" — is directly and more strongly observable at the seam, and the
seam already exists (`client.rs:136,250,255`; `http.rs:390,566,571`;
`protocol.rs:423,433,440`); (iv) the needles stay derived from
`protocol.rs:68-74`, so the guard cannot drift.

### Risks / interactions with other tests

* **`cargo clippy --all-targets --all-features -- -D warnings` (CI `lint` job,
  ci.yml:38).** If Step 2 drops any of `METHOD_INITIALIZE`,
  `METHOD_INITIALIZED`, `METHOD_TOOLS_LIST`, `METHOD_TOOLS_CALL`,
  `MCP_PROTOCOL_VERSION`, `JSONRPC_VERSION`, `decode_reply`, `decode_response` from
  the assertion bodies, the corresponding imports at
  `src/mcp/http_tests.rs:18-22` become unused → the lint job fails. Keep the
  constant-derived needles (as above) or delete the imports in the same edit.
* **`find("mod tests")` cutting early.** Safe today: neither `http.rs` nor the
  production half of `client.rs` contains the words `mod tests` in a comment
  (verified with grep over both files). If that ever changes, prefer the
  CRLF-normalized `find("#[cfg(test)]\nmod tests {")` form of the same fix.
* **`include_str!` scans are layout-dependent by design.** Related guards elsewhere
  in the repo scan sources too (e.g. `src/harness/pty_tests.rs:935,1080` and the
  `walk_src_for` guards mentioned in `docs/verification_orchestrator.md:29`) — any
  change to this test should not add new `include_str!` targets, to keep the number
  of self-scanning tests bounded.
* **Edition churn:** `2024-11-05` and `MCP_PROTOCOL_VERSION` are also asserted
  absent at `:467-470`; under the current (broken) slice those assertions fire on
  Windows too, so the whole test must be expected to flip from red to green
  *together*, not partially.
* **No production code is touched by either step**, so the MCP runtime behaviour
  (handshake order, id allocation) is unchanged; the protocol behaviour itself is
  covered by the positive protocol-layer guards in the same file
  (`src/mcp/http_tests.rs:33-39` `canonical_initialize_body`) and by
  `src/mcp/client.rs:713-741`.

---

# Verification checklist for the implementer (platform-adequate)

```bash
# Linux gate (unchanged behaviour expected)
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features

# Race regression for Site 1: must be 0 failures WITHOUT --test-threads=1
for i in $(seq 1 40); do cargo test --lib config::tests::sandbox >/dev/null 2>&1 || echo "run $i FAILED"; done
# baseline before the fix: ~33/40 failures at default threads (measured on this box)

# Site 3 regression on every platform (added to the named test)
cargo test --lib harness::search::tests::normalize_for_compare_is_identity_on_unix_and_canonical_on_windows

# Site 4: passes with LF *and* CRLF sources
cargo test --lib mcp::http_tests::tests::mcp_http_transport_keeps_no_protocol_of_its_own

# Windows compile parity for the cfg(windows) test halves (compile-only, NOT a run)
cargo check --target x86_64-pc-windows-gnu --all-targets
```

**Flag carried from the CI facts:** the green `cargo check --target
x86_64-pc-windows-gnu --all-targets` (the pattern used by earlier tasks,
`docs/verification_final.md:153`) proves only that the Windows code *compiles*. CI
runs **`x86_64-pc-windows-msvc` on `windows-latest`, executing `cargo test
--all-targets --all-features`** (`.github/workflows/ci.yml:51-52,72`). None of these
five failures can be observed with a gnu compile check, and a local `windows-gnu`
run (Wine or otherwise) is **not** equivalent: the msvc CRT and the CRLF checkout
that Site 4 depends on are both msvc-runner properties.
