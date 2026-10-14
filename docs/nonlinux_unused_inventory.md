# Inventory — non-Linux-only unused imports & dead items

**Task:** t-005 — read-only static analysis. **No source file was modified** (only `cargo check`/`cargo clippy` were run; build artifacts under `target/` were purged/rebuilt).
**Files audited (and nothing else):**

| file | lines | md5 (anchor for the line numbers below) |
|---|---|---|
| `src/mcp/client.rs` | 842 | `fd54c158929d2d9473cb83ee6807bc99` |
| `src/harness/tool_resolution_tests.rs` | 779 | `de1aa594208e66aff4b311432e87af56` |
| `src/llm/client_tests.rs` | 837 | `f26676ecbabb3c3058db8e3547394029` |

Repo state at measurement: `git rev-parse --short HEAD` → `9367552`; `git status --short` showed only `src/harness/sandbox.rs` and `src/main.rs` as modified (the `sandbox.rs` change is the t-001 Variant-A fix landing mid-session — see §Caveats). `src/mcp/http_tests.rs` md5 `2ea455c44e2e9761076bbe08524d84e4`.

Module wiring (relevant: all three files are compiled only in the test profile):
- `src/harness/mod.rs:25-26` → `#[cfg(test)] mod tool_resolution_tests;`
- `src/llm/client.rs:1088-1090` → `#[cfg(test)] #[path = "client_tests.rs"] mod tests;`
- `src/mcp/client.rs:427-428` → `#[cfg(test)]` / `mod tests {`

**Bottom line:** 13 warnings exist on Windows only; **zero on Linux**, and — because every gate in these three files is `#[cfg(unix)]` / `cfg!(windows)` (never `target_os = "linux"`) — **zero on macOS as well**, since macOS satisfies `cfg(unix)` and therefore compiles the same test bodies that Windows drops. Nothing in these three files is dead on every target, and **no `DELETE` action is safe**: every finding must be **CFG-GATED with `#[cfg(unix)]`**.

---

## Targets exercised here

```
$ rustup target list --installed
aarch64-pc-windows-gnullvm
aarch64-pc-windows-msvc
arm64ec-pc-windows-msvc
thumbv6m-none-eabi
thumbv7m-none-eabi
thumbv8m.main-none-eabihf
wasm32-unknown-unknown
x86_64-pc-windows-gnu
x86_64-pc-windows-msvc
x86_64-unknown-linux-gnu

$ rustc --version ; cargo --version
rustc 1.98.0 (88d9e12ae 2026-08-18)
cargo 1.98.0 (797e8a9bc 2026-08-05)
```

| # | command | outcome |
|---|---|---|
| 1 | `rm -rf target/debug/.fingerprint/marmennill-*` then `cargo check --all-targets` (host `x86_64-unknown-linux-gnu`) | **`Checking marmennill v1.0.0 …` / exit 0 / 0 `warning:` lines.** Baseline: nothing in these three files is unused on Linux. |
| 2 | `rm -rf target/x86_64-pc-windows-gnu/debug/.fingerprint/marmennill-*` then `cargo check --all-targets --target x86_64-pc-windows-gnu` | exit 0, **`warning: marmennill (lib test) generated 13 warnings`** — all 13 are in the three audited files (§Compiler evidence). Verified from a genuinely recompiled unit (log line 1 is `Checking marmennill v1.0.0`, not `Fresh`). |
| 3 | `cargo clippy --all-targets --target x86_64-pc-windows-gnu` | same 13 locations, **no extra clippy lint in the three files**. One unrelated clippy lint elsewhere: `clippy::unnecessary_lazy_evaluations` at `src/config.rs:537` (out of scope, listed only so the counts reconcile). |
| 4 | `rustup target add x86_64-apple-darwin` (also `aarch64-apple-darwin` not installed) | **FAILED — no network**: `error: component download failed for rust-std-x86_64-apple-darwin: error opening file for download: cleaning up cached downloads (os error 2)`. So **no macOS compile is observable here**; macOS is reasoned from the `cfg` blocks (next row). |
| 5 | macOS reasoning (no compile) | `grep -n "#\[cfg(" src/mcp/client.rs src/harness/tool_resolution_tests.rs src/llm/client_tests.rs` → the only cfg predicates in the three files are `cfg(test)` (client.rs 268/273/290/308/327/427), `cfg(unix)` (client.rs 565/636/662/688/750/791/822; tool_resolution_tests 141/190/269/437/502; llm/client_tests 684) and the runtime `cfg!(windows)` at `src/mcp/client.rs:597`. **No `target_os = "linux"`, no `target_os = "macos"`, no `cfg(not(unix))`.** Since `cfg(unix)` is `true` for `*-apple-darwin`, the macOS compilation unit is identical to the Linux one for these findings → **macOS shows the same 0 warnings as Linux**; the 13 warnings are Windows-only. |
| 6 | `cargo check --all-targets --target x86_64-pc-windows-msvc` (second Windows data point) | **Cannot reach this crate**: dependency build scripts fail before `marmennill` is checked — `error: failed to run custom build command for onig_sys v69.9.3` and `… for aws-lc-sys v0.44.0` (no Windows C toolchain on this host). `grep "Checking marmennill" /tmp/msvc_check.log` → no match. `x86_64-pc-windows-gnu` is therefore the only working Windows target here. |

---

## Unused imports

| file | line | symbol | unused on (Linux / macOS / Windows) | action | reason |
|---|---|---|---|---|---|
| `src/mcp/client.rs` | **632** (`use crate::mcp::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcTransport};`) | `JsonRpcNotification` | Windows only | **CFG-GATE** (`#[cfg(unix)]`) | Only test-side use is `JsonRpcNotification::initialized()` at 727, inside `stdio_wire_bodies_are_the_protocol_layers_envelopes` (test at 690, gated `#[cfg(unix)]` at 688). Linux/macOS: live. Windows: consumer gone → `unused_imports`. Production uses (236) are the *top-level* import at line 21, not this one — do not confuse them. |
| `src/mcp/client.rs` | **632** | `JsonRpcRequest` | Windows only | **CFG-GATE** (`#[cfg(unix)]`) | Test-side uses at 713 (`JsonRpcRequest::initialize()`) and 737 (`JsonRpcRequest::tools_list(1)`), both inside the same `#[cfg(unix)]` test. |
| `src/mcp/client.rs` | **632** | `JsonRpcTransport` | Windows only | **CFG-GATE — must NOT be deleted** | It is a **trait** (`src/mcp/protocol.rs:396 pub(crate) trait JsonRpcTransport`) whose only test-side consumer is the trait-method call `conn.request_ids().next()` at **`src/mcp/client.rs:836`**, inside `both_transports_share_the_protocols_id_allocator_policy` (test at 824, gated `#[cfg(unix)]` at 822). `request_ids` is **not** an inherent method of `StdioMcpConnection` (inherent impls are `src/mcp/client.rs:73` and `246`, which supply `spawn`/`list_tools`/`call_tool`/`shutdown`; `fn request_ids` at client.rs:150 lives inside `impl JsonRpcTransport for StdioMcpConnection` at 145). Deleting the import breaks the Linux **and** macOS build with `E0599` (see §Compiler evidence, probe 5). rustc's own `unused_imports` lint proves the point: it does **not** flag this name on Linux even though there is no plain textual use. |
| `src/mcp/client.rs` | 21 (`self, JsonRpcNotification, JsonRpcRequest, JsonRpcTransport, RequestIds, decode_reply,`) | all | none | **KEEP** | Used by production code on every target: `impl JsonRpcTransport for StdioMcpConnection` (145), `send_request(&JsonRpcRequest)` (156), `send_notification(&JsonRpcNotification)` (236). |
| `src/mcp/client.rs` | 834 (`use crate::mcp::protocol::{FIRST_REQUEST_ID, HANDSHAKE_REQUEST_ID};`) | both | none (already inside the `#[cfg(unix)]` test body) | **KEEP** | Inside the gated test body, so it disappears together with the test; already `cfg(unix)`-free of warnings. |
| `src/harness/tool_resolution_tests.rs` | 13–20 (`use super::*;`, `crate::mcp::{McpManager, McpServerConfig}`, `crate::tool_names::{…}`, `std::collections::{HashMap, HashSet}`, `std::sync::Arc`) | — | none | **KEEP** | `cargo check --target x86_64-pc-windows-gnu --all-targets` reports **no** unused import here: `HashMap` (107), `HashSet`/`Arc` (117-124), `McpManager`/`McpServerConfig` and the `tool_names` constants (207, 305, 520, 575 …) are all reached by the non-gated tests (200, 234, 324, 385, 569, 626, 668, 712, 752). |
| `src/llm/client_tests.rs` | (file-level imports) | — | none | **KEEP** | No `unused_imports` warning on the Windows cross-target. |

---

## Dead items

Every dead item below is dead **only on Windows**: each one's consumers are `#[cfg(unix)]`-gated tests/fixtures, so Linux **and** macOS keep them alive. `DELETE` would remove real macOS/Linux coverage — the fix is `#[cfg(unix)]`.

### `src/harness/tool_resolution_tests.rs`

| line | item | dead on (Linux / macOS / Windows) | action | reason (consumer evidence) |
|---|---|---|---|---|
| 24 | `const MCP_MARKER: &str = "MARKER_SENTINEL_from_scripted_mcp_server";` | Windows only | **CFG-GATE** | Used at 81 (inside `scripted_server`) and in assertions at 491 (test at 439, gate 437) and 553 (test at 504, gate 502). |
| 28 | `static REGISTRY_LOCK: std::sync::Mutex<()>` | Windows only | **CFG-GATE** | Sole use 31 (inside `registry_guard`). |
| 30 | `fn registry_guard()` | Windows only | **CFG-GATE** | Call sites 144, 272, 440, 505 — all inside `#[cfg(unix)]` tests (gates 141, 269, 437, 502). |
| 36 | `fn clear_global_mcp_registry()` | Windows only | **CFG-GATE** | Call sites 169, 296, 477, 534 — all inside the same gated tests. |
| 42 | `fn multi_thread_runtime()` | Windows only | **CFG-GATE** | Call sites 154, 192 (`boot_in_private_runtime`, itself `#[cfg(unix)]` at 190), 274, 442, 510. |
| 52 | `struct ScriptedServer { _dir, config }` | Windows only | **CFG-GATE** | Constructed at 86; referenced at 57, 106, 191 (all gated). |
| 57 | `fn scripted_server(tools: &[&str]) -> ScriptedServer` | Windows only | **CFG-GATE** | Call sites 145, 273, 441, 506. Fixture spawns POSIX tooling — `command: Some("sh")`, `cat '<path>' ; sleep 30` (lines 94-101) — so it cannot run on Windows; the `#[cfg(unix)]` gate on the consumer tests is deliberate (see the comment at 94-97 naming macOS `/usr/bin`). |
| 106 | `async fn boot_manager(scripted: &ScriptedServer, server_name: &str) -> McpManager` | Windows only | **CFG-GATE** | Call sites 156, 193, 276, 444, 512. |

**Live on all targets in this file (KEEP, do not gate):** `gate_with_allowed` (116 — used at 209, 422, 613), `invocation` (128 — used at 165, 218, 458, 463, 468, 515, 519, 524, 686, 698 …), and the non-gated tests 200, 234, 324, 385, 569, 626, 668, 712, 752. None of them is warned about on Windows.

### `src/llm/client_tests.rs`

| line | item | dead on (Linux / macOS / Windows) | action | reason |
|---|---|---|---|---|
| 636 | `struct ScriptedMcp { _dir, config }` | Windows only | **CFG-GATE** | Constructed at 661 inside `scripted_mcp`; type appears at 641. |
| 641 | `fn scripted_mcp(tools: &[&str]) -> ScriptedMcp` | Windows only | **CFG-GATE** | Sole call sites 691, 692 inside `test_policy_refused_mcp_names_are_never_advertised` (test at 686, `#[cfg(unix)]` at 684 — the only cfg gate in the whole file). Fixture is POSIX (`sh -c "cat … ; sleep 30"`, 674-679). |
| 678 | `static MCP_REGISTRY_LOCK: tokio::sync::Mutex<()>` | Windows only | **CFG-GATE** | Sole use 687 (same gated test). Note the doc comment at 677 refers to "This test" — it belongs to the gated test and should be gated with it. |

### `src/mcp/client.rs` (tests module)

| line | item | status | action |
|---|---|---|---|
| 637 | `async fn read_capture_lines(…)` | already `#[cfg(unix)]` at 636 | KEEP (correct precedent — this is the pattern to copy for line 632) |
| 663 | `fn scripted_server(lines: &[&str]) -> McpServerConfig` | already `#[cfg(unix)]` at 662 | KEEP |
| — | no other dead item | `cargo check --target x86_64-pc-windows-gnu` reports none | — |

### Why not `DELETE`, and why not "un-gate the tests instead"?

The sandbox audit (`docs/sandbox_deadcode_audit.md`, Variant A) offered "remove the `#[cfg(target_os = "linux")]` on the consumer test" as an alternative. **That alternative is unavailable here**: these fixtures launch POSIX programs (`sh`, `cat`, `sleep`, `tee`, `printf`), which Windows does not provide — the source comments say so explicitly (`src/mcp/client.rs:562-565`, `659-661`, `684-687`, `820-822`; `src/harness/tool_resolution_tests.rs:94-97`; `src/llm/client_tests.rs:674-679`). So `#[cfg(unix)]` on the helpers is the only correct action. Also verified: no `#[allow…]`/`#[expect…]`/`#[deprecated]` attribute exists in any of the three files (`grep -n "allow(" → no hits`), so nothing is being hidden from the lints.

---

## Compiler evidence

All blocks below are verbatim from `cargo check --all-targets --target x86_64-pc-windows-gnu` on a purged fingerprint (log `/tmp/win_final.log`; total `warning: marmennill (lib test) generated 13 warnings`).

```
warning: unused imports: `JsonRpcNotification` and `JsonRpcRequest`
   --> src/mcp/client.rs:632:32
    |
632 |     use crate::mcp::protocol::{JsonRpcNotification, JsonRpcRequest, Jso...
    |                                ^^^^^^^^^^^^^^^^^^^  ^^^^^^^^^^^^^^
    |
    = note: `#[warn(unused_imports)]` (part of `#[warn(unused)]`) on by default

warning: unused import: `JsonRpcTransport`
   --> src/mcp/client.rs:632:69
    |
632 | ...fication, JsonRpcRequest, JsonRpcTransport};
    |                              ^^^^^^^^^^^^^^^^
```

```
warning: constant `MCP_MARKER` is never used
  --> src/harness/tool_resolution_tests.rs:24:7
   |
24 | const MCP_MARKER: &str = "MARKER_SENTINEL_from_scripted_mcp_server";
   |       ^^^^^^^^^^
   |
   = note: `#[warn(dead_code)]` (part of `#[warn(unused)]`) on by default

warning: static `REGISTRY_LOCK` is never used
  --> src/harness/tool_resolution_tests.rs:28:8
   |
28 | static REGISTRY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
   |        ^^^^^^^^^^^^^

warning: function `registry_guard` is never used
  --> src/harness/tool_resolution_tests.rs:30:4
   |
30 | fn registry_guard() -> std::sync::MutexGuard<'static, ()> {
   |    ^^^^^^^^^^^^^^

warning: function `clear_global_mcp_registry` is never used
  --> src/harness/tool_resolution_tests.rs:36:4
   |
36 | fn clear_global_mcp_registry() {
   |    ^^^^^^^^^^^^^^^^^^^^^^^^^

warning: function `multi_thread_runtime` is never used
  --> src/harness/tool_resolution_tests.rs:42:4
   |
42 | fn multi_thread_runtime() -> tokio::runtime::Runtime {
   |    ^^^^^^^^^^^^^^^^^^^^

warning: struct `ScriptedServer` is never constructed
  --> src/harness/tool_resolution_tests.rs:52:8
   |
52 | struct ScriptedServer {
   |        ^^^^^^^^^^^^^^

warning: function `scripted_server` is never used
  --> src/harness/tool_resolution_tests.rs:57:4
   |
57 | fn scripted_server(tools: &[&str]) -> ScriptedServer {
   |    ^^^^^^^^^^^^^^^

warning: function `boot_manager` is never used
   --> src/harness/tool_resolution_tests.rs:106:10
    |
106 | async fn boot_manager(scripted: &ScriptedServer, server_name: &str) -> ...
    |          ^^^^^^^^^^^^
```

```
warning: struct `ScriptedMcp` is never constructed
   --> src/llm/client_tests.rs:636:8
    |
636 | struct ScriptedMcp {
    |        ^^^^^^^^^^^

warning: function `scripted_mcp` is never used
   --> src/llm/client_tests.rs:641:4
    |
641 | fn scripted_mcp(tools: &[&str]) -> ScriptedMcp {
    |    ^^^^^^^^^^^^

warning: static `MCP_REGISTRY_LOCK` is never used
   --> src/llm/client_tests.rs:678:8
    |
678 | static MCP_REGISTRY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::...
    |        ^^^^^^^^^^^^^^^^^
```

Host baseline (Linux), same fingerprint purge, verbatim:

```
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.80s
```
(exit 0; `grep -c "^warning"` over the whole output → `0`.)

**Probe 5 — why `JsonRpcTransport` (client.rs:632) may not be deleted** (`JsonRpcTransport` is `pub(crate)`, so it cannot be probed from outside the crate; the probe reproduces the exact language rule with the same shape: trait method call + trait import inside a `#[cfg(test)] mod tests`):

```
$ rustc --edition 2021 --test /tmp/trait_probe/without.rs
error[E0599]: no method named `request_ids` found for struct `StdioMcpConnectionLike` in the current scope
  = help: items from traits can only be used if the trait is in scope
help: trait `JsonRpcTransportLike` which provides `request_ids` is implemented but not in scope; perhaps you want to import it
```

The same probe with the trait in scope compiles and runs (`rustc … with.rs`, exit 0). Cross-check inside the crate: rustc does **not** report `JsonRpcTransport` as unused on Linux while it **does** report the two type imports on Windows — i.e. the lint counts the trait-method use at `src/mcp/client.rs:836`, which proves the import is load-bearing on every `cfg(unix)` target.

---

## Protected literals

Two source-text scans read these files as **text**, independent of `cfg`. Any deletion/edits must respect them.

### A. `src/mcp/http_tests.rs:444-479` — `mcp_http_transport_keeps_no_protocol_of_its_own`

```
445  fn production_only(src: &str) -> &str {
446      src.split("#[cfg(test)]\nmod tests {").next().unwrap_or(src)
447  }
448  for src in [
449      production_only(include_str!("http.rs")),
450      production_only(include_str!("client.rs")),
451  ] {
452      assert!(!src.contains("jsonrpc:\"2.0\"") && !src.contains("jsonrpc: \"2.0\""), …);
456      for method in [METHOD_INITIALIZE, METHOD_INITIALIZED, METHOD_TOOLS_LIST, METHOD_TOOLS_CALL] {
462          assert!(!src.contains(&format!("\"{method}\"")),
463              "the transports must not spell out the method name '{method}'");
466      }
467      assert!(!src.contains("MCP_PROTOCOL_VERSION") && !src.contains("2024-11-05"), …);
471      assert!(!src.contains("AtomicU64"), …);
475      assert!(!src.contains("\"inputSchema\"") && !src.contains("\"isError\""), …);
```

These are **negative** assertions over the *production* half only (`client.rs:1..=426`). Measured current state of that half (`head -n 426 src/mcp/client.rs | grep -cF …`): every forbidden token count is **0**, so the test passes today.

Consequences for the implementer:

1. **The boundary sequence `#[cfg(test)]\nmod tests {` at `src/mcp/client.rs:427-428` is the load-bearing literal.** It must survive byte-for-byte (`#[cfg(test)]` on its own line, immediately followed by `mod tests {` — verified with `cat -A`: no trailing spaces, no blank line between them). If it ever fails to match, `unwrap_or(src)` silently feeds the **whole file** to the assertions, and the test then fails at **line 462** with `the transports must not spell out the method name 'initialize'`, because the tests module quotes the wire text at `src/mcp/client.rs:719` (`r#"{"jsonrpc":"2.0","id":0,"method":"initialize",… "protocolVersion":"2024-11-05"}}…`), plus 733 (`"notifications/initialized"`), 741 (`"tools/list"`), 754-758, 795-796. Concretely: do **not** insert anything between 427 and 428, do not collapse them onto one line, do not add an attribute (e.g. `#[cfg(unix)]`, `#[allow(…)]`) between them, and do not rename `mod tests`.
2. **Do not move any test code above line 427.** If a helper with quoted wire text (e.g. `scripted_server`, 663) were relocated above the boundary, `"tools/list"`/`"initialize"`/`"tools/call"`/`"notifications/initialized"`/`2024-11-05`/`jsonrpc:"2.0"`/`"inputSchema"`/`"isError"` would enter the scanned half and trip 452/462/467/471/475.
3. Deleting code from the tests module (lines 428-842) is **safe** for this scan — that half is never scanned. All the proposed `#[cfg(unix)]` insertions for `client.rs` sit at line 632, far below the boundary.
4. `src/mcp/http.rs` is scanned the same way (line 449) — out of scope for this inventory, but do not "share" protocol literals from client.rs into http.rs.

### B. `tests/test_tool_name_literals.rs:31-… ` — repo-wide raw-literal guard (runtime `std::fs` scan of every `src/**/*.rs`, production **and** test halves)

- Baseline row **`tests/test_tool_name_literals.rs:67`: `("src/mcp/client.rs", "sleep", 1),`** ↔ the single raw literal `"sleep"` at **`src/mcp/client.rs:605`** (`("sleep".to_string(), vec!["30".to_string()])`, inside the cross-platform test `stdio_read_hits_the_shared_pump_deadline`, 595 — **not** unix-gated). The test asserts an *exact* count (`count_literal` counts `"{tool_name}"` occurrences), so deleting, quoting-through-`crate::tool_names`, or duplicating that `"sleep"` string **fails the guard** unless the baseline row is updated in the same change.
- `src/harness/tool_resolution_tests.rs` and `src/llm/client_tests.rs` currently hold **0** raw tool-name literals of the `KNOWN_TOOL_NAMES` list (measured: only `"sleep"` ×1 in `client.rs`, nothing in the other two). So while gating/reshuffling their fixtures, **do not introduce** raw `"read_file"`, `"run_command"`, `"glob"`, `"grep_search"`, `"list_directory"`, `"terminal__read_file"` … literals — keep using `crate::tool_names::*` (as `src/harness/tool_resolution_tests.rs:15-18` and `src/llm/client_tests.rs:691` already do). `"MARKER_SENTINEL_from_scripted_mcp_server"` (line 24) and `"myserver__do_thing"` are not in the known-name list, so they are unrestricted.
- No scan anywhere reads `src/harness/tool_resolution_tests.rs` or `src/llm/client_tests.rs` as text (`grep -rn "include_str!\|client.rs" src tests` finds only `src/mcp/http_tests.rs:450` plus doc-comment mentions). The only other `include_str!` source scans in the repo are unrelated (`src/orchestrator/steer_tests.rs:1111`, `src/agents/validation.rs:640`, `src/agents/runner/fix_loop.rs:1910`, `src/manager/context_tests.rs:2066`, `src/mcp/http_tests.rs:405/449`, `tests/test_steer_history.rs:306`).

---

## Suggested minimal diff outline

Insert `#[cfg(unix)]` — one attribute per item, placed directly above the item (after its doc comment, or above the doc block; both compile). Apply **bottom-up within each file** so the quoted line numbers stay valid. Total: 14 attributes, 0 deletions.

**`src/mcp/client.rs`** (1 attribute)
```
631:                                  # + #[cfg(unix)]
632:      use crate::mcp::protocol::{JsonRpcNotification, JsonRpcRequest, JsonRpcTransport};
```
Nothing else in `client.rs` changes: line 21 and line 834 stay as-is, and lines 427-428 / 605 are protected (see §Protected literals).

**`src/harness/tool_resolution_tests.rs`** (8 attributes)
```
before  24:  #[cfg(unix)]   const MCP_MARKER: &str = …
before  28:  #[cfg(unix)]   static REGISTRY_LOCK: std::sync::Mutex<()> = …
before  30:  #[cfg(unix)]   fn registry_guard() -> std::sync::MutexGuard<'static, ()>
before  36:  #[cfg(unix)]   fn clear_global_mcp_registry()
before  42:  #[cfg(unix)]   fn multi_thread_runtime() -> tokio::runtime::Runtime
before  52:  #[cfg(unix)]   struct ScriptedServer { … }
before  57:  #[cfg(unix)]   fn scripted_server(tools: &[&str]) -> ScriptedServer
before 106:  #[cfg(unix)]   async fn boot_manager(scripted: &ScriptedServer, server_name: &str) -> McpManager
```
Leave 116 (`gate_with_allowed`) and 128 (`invocation`) **ungated** — they are live on Windows too.

**`src/llm/client_tests.rs`** (3 attributes)
```
before 636:  #[cfg(unix)]   struct ScriptedMcp { … }
before 641:  #[cfg(unix)]   fn scripted_mcp(tools: &[&str]) -> ScriptedMcp
before 678:  #[cfg(unix)]   static MCP_REGISTRY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
```

Rejected alternative (recorded for the implementer): wrapping the fixtures in a single `#[cfg(unix)] mod fixtures { … }`. It silences the same warnings but changes every call site (`scripted_server(…)` → `fixtures::scripted_server(…)`, `MCP_MARKER` → `fixtures::MCP_MARKER`) and pushes a 120-line block around; per-item attributes match the precedent already in `src/mcp/client.rs:636/662` and the accepted `docs/sandbox_deadcode_audit.md` Variant A.

**Verification the implementer should run after the change**
1. `cargo check --all-targets` → must stay **0 warnings** (Linux: nothing may become unused).
2. `rm -rf target/x86_64-pc-windows-gnu/debug/.fingerprint/marmennill-* && cargo check --all-targets --target x86_64-pc-windows-gnu` → **0 warnings** (was 13).
3. `cargo clippy --all-targets` and `cargo clippy --all-targets --target x86_64-pc-windows-gnu` → no new lint (the pre-existing `src/config.rs:537` `unnecessary_lazy_evaluations` is unrelated).
4. `cargo test --lib mcp_http_transport_keeps_no_protocol_of_its_own` → green (boundary literal at `client.rs:427-428` intact).
5. `cargo test --test test_tool_name_literals` → green (`"sleep"` ×1 at `client.rs:605` intact).
6. `cargo test --lib` on Linux → the 4 `cfg(unix)` tests in `tool_resolution_tests` plus `test_policy_refused_mcp_names_are_never_advertised` and the `cfg(unix)` stdio tests in `mcp::client` must still **run** (count them before/after: gating must not silently disable them on Linux/macOS).

**Caveats / drift note**
- Line numbers were measured against the md5 anchors above. `src/harness/sandbox.rs` moved by +1 line during this session (its `#[cfg(target_os = "linux")]` gate at 498-499 landed mid-run: the cached run warned at `sandbox.rs:499/510/512/535`, the post-change run reports **zero** `sandbox.rs` warnings). The three audited files were untouched (`git status`), so their numbers are stable — re-anchor with `md5sum` before editing.
- macOS was never compiled here (no network for `rust-std-*-apple-darwin`, no MSVC Windows C toolchain). The macOS conclusion rests on the exhaustive cfg census in §Targets row 5: `cfg(unix)` is `true` on `x86_64-apple-darwin`, so the macOS `dead_code`/`unused_imports` set for these three files equals the Linux set (empty). If a future gate uses `target_os = "linux"` inside these files, re-run the inventory.

MISSION COMPLETE — report: `docs/nonlinux_unused_inventory.md`
