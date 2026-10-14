# Audit — unused imports & dead code in `src/harness/sandbox.rs`

**Task:** t-001 (research / static analysis only — **no source file was modified**).
**File audited:** `src/harness/sandbox.rs` (1268 lines, 50 861 bytes).
**Crate:** `marmennill` v1.0.0, edition 2024 (`src/lib.rs:7 pub mod harness;`, `src/harness/mod.rs:22 pub mod sandbox;`).
**Dev box:** Linux (`x86_64-unknown-linux-gnu`). Every `#[cfg(target_os = "linux")]` path below is compiled and verified on this box. Non-Linux paths were **also verified for real** using the installed cross-target `x86_64-pc-windows-gnu` (see §Compiler evidence). No `*-apple-darwin` target is installed (`rustup target list --installed` → `aarch64-pc-windows-gnullvm`, `aarch64-pc-windows-msvc`, `arm64ec-pc-windows-msvc`, `thumbv6m/7m/8m`, `wasm32-unknown-unknown`, `x86_64-pc-windows-gnu`, `x86_64-pc-windows-msvc`, `x86_64-unknown-linux-gnu`), so a **macOS** build is not directly observable — the Windows cross-target stands in for the `#[cfg(not(target_os = "linux"))]` half, which is the same cfg half macOS uses.

Headline: **the file is clean on Linux — zero warnings from `cargo check --all-targets` and zero from `clippy`.** All findings are cfg-conditional (they only appear when `target_os != "linux"`) plus a set of `pub` items that are not dead but are never referenced outside this file (surface area, not deletions).

---

## Unused imports

| symbol | import line | cfg gate | verdict | suggested edit |
|---|---|---|---|---|
| `crate::harness::pty::SANDBOX_OPTOUT_ENV` | 34 | none | **USED** — 192, 201, 285 (non-test) + 694, 708, 730, 737, 741, 800, 982, 1048, 1064, 1086, 1107, 1128, 1134, 1160, 1210 | keep |
| `SandboxDecision` | 34 | none | **USED** — 241, 246, 254, 258, 261, 265 (non-test) + 670, 671, 709, 734, 745, 752, 760 | keep |
| `SandboxSkip` | 34 | none | **USED** — 247 (non-test `reentry_action`) + 711, 736, 747, 754 | keep |
| `is_optout_value` | 34 | none | **USED** — 200 (`opt_out_from_value` guard) | keep |
| `std::path::Path` | 35 | none | **USED** — 157, 298, 332, 456, 344/357/362/424/440 (`Path::new`), 487 (non-linux `attempt_landlock`), 900, 928, 938, 973 | keep — required on **both** target halves |
| `landlock::ABI` | 39 | `target_os = "linux"` | **USED** — 299, 332 (signature), 1009, 1241 | keep |
| `landlock::Access` | 39 | `target_os = "linux"` | **NOT textually referenced, but REQUIRED** — `AccessFs::from_all` is a *trait* method (`impl Access for AccessFs`, `landlock-0.4.7/src/fs.rs:99-107`); removing it breaks the Linux build | **DO NOT remove** (see §Do NOT remove) |
| `landlock::AccessFs` | 39 | linux | **USED** — 334, 351, 362, 375, 384, 397, 407, 429, 445, 1009, 1243, 1252, 1261 | keep |
| `landlock::PathBeneath` | 39 | linux | **USED** — 351, 362, 375, 384, 397, 407, 429, 445, 457, 1252, 1261 | keep |
| `landlock::PathFd` | 39 | linux | **USED** — 344, 357, 373, 382, 392, 402, 424, 440, 457, 992, 1248, 1257 | keep |
| `landlock::Ruleset` | 39 | linux | **USED** — 333, 1008, 1242 | keep |
| `landlock::RulesetAttr` | 39 | linux | **NOT textually referenced, but REQUIRED** — provides `handle_access` (`ruleset.rs:342/350`) | **DO NOT remove** |
| `landlock::RulesetCreated` | 39 | linux | **USED** — 332, 455, 458 (types) | keep |
| `landlock::RulesetCreatedAttr` | 40 | linux | **NOT textually referenced, but REQUIRED** — provides `add_rule` (`ruleset.rs:749/755`) | **DO NOT remove** |
| `landlock::RulesetStatus` | 40 | linux | **USED** — 310, 311, 316 | keep |
| `std::path::PathBuf` | 43 | `target_os = "linux"` | **USED** — 368, 390, 400 (`std::env::var_os(..).map(PathBuf::from)`) | keep |
| `super::*` (tests) | 498 | none | **USED** — supplies `apply_sandbox`, `SandboxAttempt`, `SandboxError`, `reentry_action`, `Enforcement`, `ReentryAction`, `refusal_message`, `opt_out_from_value`, `add_rule`, and (on Linux) `PathBuf`/`ABI`/`Ruleset`/`PathFd`/`PathBeneath`/`RulesetStatus` | keep |
| `crate::harness::pty::SANDBOX_EXEC_ARG` (tests) | **499** | **none** | **UNUSED on non-Linux** — all 8 identifier uses (979, 1020, 1045, 1083, 1125, 1157, 1166, 1207) live inside `#[cfg(target_os = "linux")]` items; the two mentions at 855/858 are **string literals**, not uses. Confirmed by the compiler on `x86_64-pc-windows-gnu` | **Gate it:** insert `#[cfg(target_os = "linux")]` on a new line before 499. Do **not** delete the import (Linux needs it) |
| `std::path::PathBuf` (tests) | 500 | none | **USED on every target** — non-gated uses at 672, 710, 735, 746; gated uses at 901, 911, 915, 1017. Redundant-looking on Linux (also arrives via `use super::*`), but on non-Linux the parent's `PathBuf` import is cfg'd out, so this one is load-bearing | keep |
| `std::os::unix::fs::PermissionsExt` (inside `is_executable`) | 929 | inside `#[cfg(target_os = "linux")] fn is_executable` (927/928) | **USED** — 932 (`permissions().mode()`) | keep |

**Net result: exactly one unused import, and only when `target_os != "linux"` (line 499). Zero unused imports on Linux.**

---

## Dead code

No `#[allow(dead_code)]`, `#[allow(unused)]`, `#[allow(unused_imports)]`, or `#[deprecated]` attribute exists anywhere in `src/harness/sandbox.rs` (`grep -n "#\[allow|#\[expect|deprecated"` → no hits), so nothing is being hidden from the lints.

| item | line range | evidence it is unused | suggested edit |
|---|---|---|---|
| `struct CaptureWriter` (+ `impl<'writer> tracing_subscriber::fmt::MakeWriter for CaptureWriter`) | 509-510; impl 528-533 | **Never constructed when `target_os != "linux"`** — compiler: `warning: struct 'CaptureWriter' is never constructed --> src/harness/sandbox.rs:510:12` (Windows cross-target). Its only consumer chain is `capture_logs` → `test_apply_sandbox_fails_closed_on_an_unusable_workspace_root`, which is `#[cfg(target_os = "linux")]` (643) | Preferred: delete line **643** (the `#[cfg(target_os = "linux")]` above that test) so the helpers become live everywhere. Alternative: gate 509-510 and 528-533 with `#[cfg(target_os = "linux")]` |
| `struct CaptureSink` (+ `impl std::io::Write for CaptureSink`) | 512; impl 514-526 | compiler: `warning: struct 'CaptureSink' is never constructed --> src/harness/sandbox.rs:512:12` (Windows cross-target); clean on Linux | same as above |
| `fn capture_logs<R>(…)` | 535-545 | compiler: `warning: function 'capture_logs' is never used --> src/harness/sandbox.rs:535:8` (Windows cross-target). Sole call site is 649, inside the Linux-gated test | same as above |
| `thread_local! { static CAPTURED }` | 504-507 | **Not reported dead** — the `Write for CaptureSink` impl (516) counts as a use even when `CaptureSink` is never constructed. No warning on either target | no action (if the gate-the-helpers variant is chosen, gate it together with 509-545 for tidiness) |
| everything else private: `refuse` (272), `attempt_landlock` linux (298) / non-linux (487), `build_ruleset` (332), `add_rule` (454), `error_chain` (470), `apply_decision` (670), `reentry_exe` (900), `discover_reentry_exe` (911), `is_executable` (928), `candidate_is_current` (938), `implements_reentry` (973), `kernel_has_landlock` (1005), `integration_host` (1017) | — | each has a live caller: `refuse`←258/261/265; `attempt_landlock`←158; `build_ruleset`←301; `add_rule`←353/360/375/384/395/405/427/443 (+1250, 1259); `error_chain`←318/336/340/346/…/461 (+993); `reentry_exe`←1018; `discover_reentry_exe`←902; `is_executable`←920; `candidate_is_current`←922; `implements_reentry`←925; `kernel_has_landlock`←1029, 1220; `integration_host`←1041, 1078, 1120, 1153, 1177 | **no action — all live** |
| test-only helpers `apply_decision` (670) etc. | 670-674 | used at 680, 704, 727 … non-gated → live on every target | no action |

### Not dead, but `pub` with no consumer outside this file (surface area — flag only, do not delete)

Because `harness` and `sandbox` are `pub mod` (`src/lib.rs:7`, `src/harness/mod.rs:22`), the `dead_code` lint **cannot** fire for these; they were checked by hand with `grep -rn --include=*.rs`.

| item | lines | cross-module reference status |
|---|---|---|
| `pub enum SandboxAttempt` (+ 6 variants) | 98-117 | referenced **only** in `sandbox.rs` (`src/harness/sandbox.rs` is the single file matched by `grep -rln '\bSandboxAttempt\b' src tests`). Cannot be made private while `pub fn sandbox_outcome(attempt: &SandboxAttempt)` (125) stays `pub` |
| `pub fn sandbox_outcome` | 125 | only caller is `apply_sandbox` (158) + tests. Not referenced from `main.rs`/`config.rs`/`tests/` |
| `pub fn opt_out_from_value` | 198 | only caller is `opt_out_from_env` (192) + tests |
| `pub enum SandboxError` / `pub fn reason()` | 57-90 / 83-89 | `SandboxError` is the error type of `apply_sandbox`, consumed implicitly in `src/main.rs:121` (`Err(err) => …err.to_string()`); `kind()` has a live non-test caller at 178; `reason()` has **no** non-test caller (582, 659 are tests) but is public error API — keep |
| `pub enum Enforcement`, `pub enum ReentryAction` | 210-231 | constructed/matched in `src/main.rs:115-129` → live |

---

## Compiler/clippy evidence

All commands run in `/home/fredrik/marmel/wip`; the local crate's fingerprints were purged (`rm -rf target/debug/.fingerprint/marmennill-*`) before the first run so the unit was genuinely re-compiled (cargo then reported `Checking marmennill …`, not `Fresh`). No source file was touched.

**1. `cargo check --all-targets` (Linux) — 0 warnings:**
```
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 2.99s
```
(exit 0; no `warning:` line in the whole output.)

**2. `cargo clippy --all-targets` (Linux) — 0 warnings:**
```
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 17.29s
```
(`grep -E "unused|dead_code|never used|never read"` over the full output → no matches.)

**3. `cargo check --target x86_64-pc-windows-gnu --all-targets` — the only place `sandbox.rs` warns (verbatim, `warning: marmennill (lib test) generated 17 warnings` total across the crate):**
```
warning: unused import: `crate::harness::pty::SANDBOX_EXEC_ARG`
   --> src/harness/sandbox.rs:499:9
    |
499 |     use crate::harness::pty::SANDBOX_EXEC_ARG;
    |         ^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^^
    |
    = note: `#[warn(unused_imports)]` (part of `#[warn(unused)]`) on by default

warning: struct `CaptureWriter` is never constructed
   --> src/harness/sandbox.rs:510:12
    |
510 |     struct CaptureWriter;
    |            ^^^^^^^^^^^^^
    |
    = note: `#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default

warning: struct `CaptureSink` is never constructed
   --> src/harness/sandbox.rs:512:12
    |
512 |     struct CaptureSink;
    |            ^^^^^^^^^^^

warning: function `capture_logs` is never used
   --> src/harness/sandbox.rs:535:8
    |
535 |     fn capture_logs<R>(f: impl FnOnce() -> R) -> (R, String) {
    |        ^^^^^^^^^^^^
```
`cargo clippy --target x86_64-pc-windows-gnu --all-targets` produced the **same four** `sandbox.rs` locations (499, 510, 512, 535) and no extra clippy lint for this file. `cargo check --target x86_64-pc-windows-gnu --lib` (tests excluded) → **0 warnings**, i.e. the whole non-Linux production half of `sandbox.rs` is live.

**4. Trait-import probes (proof that `Access`, `RulesetAttr`, `RulesetCreatedAttr` are load-bearing, run with `rustc --edition 2021 --extern landlock=target/debug/deps/liblandlock-5fbea896d1efe18d.rlib` on scratch files under `/tmp`, never in the workspace):**
```
error[E0599]: no variant, associated function, or constant named `from_all` found for enum `AccessFs` in the current scope
  = help: items from traits can only be used if the trait is in scope
help: trait `Access` which provides `from_all` is implemented but not in scope; perhaps you want to import it
  |
2 + use landlock::Access;

error[E0599]: no method named `handle_access` found for struct `Ruleset` in the current scope
help: trait `RulesetAttr` which provides `handle_access` is implemented but not in scope; perhaps you want to import it
  |
2 + use landlock::RulesetAttr;

error[E0599]: no method named `add_rule` found for struct `RulesetCreated` in the current scope
help: trait `RulesetCreatedAttr` which provides `add_rule` is implemented but not in scope; perhaps you want to import it
  |
2 + use landlock::RulesetCreatedAttr;
```
Corroborating source: `~/.cargo/registry/src/…/landlock-0.4.7/src/access.rs:12` (`pub trait Access`), `…/fs.rs:99-107` (`impl Access for AccessFs { fn from_all(abi: ABI) }` — so `AccessFs::from_all` needs the trait in scope, while `AccessFs::from_read` at `fs.rs:114` is an inherent `pub fn`), `…/ruleset.rs:342/350` (`pub trait RulesetAttr … fn handle_access`), `…/ruleset.rs:749/755` (`pub trait RulesetCreatedAttr … fn add_rule`).

**cfg caveat to state out loud:** on this Linux dev box `cargo check --all-targets`/`clippy` are **silent** for `sandbox.rs`, so a Linux-only verification pass would wrongly conclude "nothing to do". The four findings above only materialize when `target_os != "linux"`; `x86_64-pc-windows-gnu` is the substitute used here (macOS targets are not installed, and macOS would exercise the same `#[cfg(not(target_os = "linux"))]` half).

---

## Do NOT remove

| item | line(s) | why it looks unused / why it is not |
|---|---|---|
| `landlock::Access` | 39 | zero textual occurrences of the identifier, but it is the trait that provides `AccessFs::from_all`, called at 334, 351, 362, 375, 397, 429, 1009, 1243, 1252, 1261. Removal → `E0599` on Linux (probe 4) |
| `landlock::RulesetAttr` | 39 | zero textual occurrences; provides `handle_access` (334, 1009, 1243). Removal → `E0599` |
| `landlock::RulesetCreatedAttr` | 40 | zero textual occurrences; provides `add_rule` (455 via `ruleset.add_rule`, 1250, 1259 in tests). Removal → `E0599` |
| `std::path::PathBuf` | 43 | used at 368/390/400 inside `#[cfg(target_os = "linux")] build_ruleset`; already gated, no warning either way |
| `use std::path::PathBuf;` (tests) | 500 | looks duplicated with `use super::*`, but the parent import is Linux-gated, so on non-Linux this is the only source of `PathBuf` used at 672/710/735/746. No warning on either target |
| `use crate::harness::pty::SANDBOX_EXEC_ARG;` (tests) | 499 | must be **gated, not deleted**: 8 real uses exist on Linux (979, 1020, 1045, 1083, 1125, 1157, 1166, 1207) |
| `apply_sandbox`, `reentry_action`, `refusal_message`, `optout_warning`, `Enforcement`, `ReentryAction`, `SANDBOX_REFUSE_EXIT_CODE` | 157, 241, 282, 293, 210, 222, 50 | consumed by `src/main.rs:96-133` (`opt_out_from_env` 99, `optout_warning` 113, `Enforcement::{Skipped,Established,Failed}` 115/120/121, `apply_sandbox` 124, `reentry_action` 127, `ReentryAction::{Exec,Refuse}` 128/129, `refusal_message` 131, exit code 133). `SANDBOX_REFUSE_EXIT_CODE` is also named in `src/config.rs:142` (doc comment) and used by `main.rs`. Deleting any of them breaks the bin and/or the guard tests at 811-846 |
| `opt_out_from_env` | 191 | real cross-module consumers: `src/main.rs:99`, `src/config.rs:892, 929, 968, 988, 1000`, plus guard tests `src/config.rs:1092, 1101` |
| `SandboxDecision`, `SandboxSkip`, `is_optout_value`, `SANDBOX_OPTOUT_ENV` imports | 34 | all four used by non-gated code (200-201, 241-265); the same types are the input contract of `reentry_action` |
| `attempt_landlock` (non-linux variant) | 487 | required by `#[cfg(not(target_os = "linux"))]`; verified live by the 0-warning Windows `--lib` check |
| `error_chain`, `build_ruleset`, `add_rule` | 470, 332, 454 | live Linux-only helpers (called from 301/353/… and from Linux-gated tests 991-1013, 1232-1268); their definitions are already gated |
| `SandboxError::reason()` / `SandboxError::kind()` | 83, 74 | `kind()` is called at 178 (production); `reason()` is called by tests 582/659 and is public error API — deleting it breaks those tests |
| `SandboxAttempt` / `sandbox_outcome` / `opt_out_from_value` | 98, 125, 198 | no cross-module caller, but each is used inside the file (158 → 301/487; 192) and is the documented injection seam for the fail-closed tests. Reducing them to `pub(crate)` would additionally require downgrading `sandbox_outcome`'s public signature (private type in public interface) — out of scope for a dead-code pass |
| `CAPTURED` static | 504-507 | never flagged; used by `CaptureSink::write` (516) and `capture_logs` (541, 543) |

---

## Suggested minimal diff outline

Two variants. **Variant A is recommended**: it deletes nothing, silences all four non-Linux warnings, and *adds* coverage (the "an enforcement failure is never silent" assertion currently never runs on non-Linux, even though the non-Linux `attempt_landlock` at 487-493 produces exactly the `SandboxError::Unavailable` that the test already accepts at 654).

**Variant A — de-gate one test, gate one import (2 edits, no deletions)**
- Insert `#[cfg(target_os = "linux")]` on a new line immediately before line **499** (`use crate::harness::pty::SANDBOX_EXEC_ARG;`). Safe: every identifier use of `SANDBOX_EXEC_ARG` is inside a `#[cfg(target_os = "linux")]` item; 855/858 are string literals. Safe on Linux (import still present) and on non-Linux (import absent, no warning).
- Delete line **643** (`    #[cfg(target_os = "linux")]`, directly above `#[test] fn test_apply_sandbox_fails_closed_on_an_unusable_workspace_root`). The body (646-665) is platform-neutral: `tempfile::tempdir` (dev-dependency, unconditional), `apply_sandbox`, `capture_logs`, `SandboxError::{RuleSetup, Unavailable}`, `err.reason()`, and the log string `"Landlock sandbox NOT established"` emitted unconditionally at 176-180. This makes `capture_logs` / `CaptureWriter` / `CaptureSink` live on all targets → warnings at 510, 512, 535 disappear.
  - Risk to check during implementation: on non-Linux the test must take the `Unavailable` branch (it does, via the `matches!` at 651-657) and the `tracing_subscriber` capture must see the `tracing::error!` — verify with `cargo test --lib` on Linux **and** a non-Linux run/compile (`cargo check --target x86_64-pc-windows-gnu --all-targets`; executing non-Linux tests is not possible on this box, so a compile-only check is the practical gate).
  - Do **not** change the module-level guard test at 870 (`test_sandbox_tests_never_rebuild_the_binary_path_from_a_name`): it asserts this file still contains `SKIP `, `discover_reentry_exe`, `implements_reentry` — those stay.

**Variant B — if the team insists on removing code rather than widening the test (5 edits, coverage loss)**
- Add `#[cfg(target_os = "linux")]` to each of: the `thread_local!` block 504-507, `struct CaptureWriter` 509-510, `struct CaptureSink` 512, `impl std::io::Write for CaptureSink` 514-526, `impl MakeWriter for CaptureWriter` 528-533, and `fn capture_logs` 535-545 (or wrap the whole block 502-545 in one `#[cfg(target_os = "linux")] mod log_capture { … }` with `use super::*;`).
- Plus the same gate on line 499 as in Variant A.
- Downside: no log-capture helper remains available for a non-Linux test, and the diff is 6 attributes instead of 2 edits.

**Do not touch (would break the build):** `landlock::Access`, `landlock::RulesetAttr`, `landlock::RulesetCreatedAttr` (lines 39-40); `std::path::Path` (35); `std::path::PathBuf` (43 and 500); `use super::*` (498); every `pub` item listed in §Do NOT remove.

**Acceptance gate for the follow-up implementation task:**
```
cargo check --all-targets                                  # 0 warnings (Linux)
cargo clippy --all-targets                                 # 0 warnings (Linux)
cargo check --target x86_64-pc-windows-gnu --all-targets   # 0 sandbox.rs warnings (expect 13 unrelated ones in src/mcp/client.rs, src/harness/tool_resolution_tests.rs, src/llm/client_tests.rs)
cargo test --lib harness::sandbox                          # Linux: all sandbox tests pass
```
