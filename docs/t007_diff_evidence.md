# t-007 - audit in ONE read: the complete change set

Baseline `HEAD = 974b601`. Scope = 16 files (9 touched, 7 byte-identical: src/llm/client.rs, src/llm/mod.rs, src/llm/stream.rs, src/mcp/http.rs, src/net/http.rs, src/net/mod.rs, src/net/sse.rs).
Comment lines: **29 removed, 36 added**. Everything t-007 changed is in the diff at the bottom
(`git diff -U1 HEAD -- src/llm src/net src/mcp`); every other byte of the scope equals
`git show HEAD:<file>`, so no source file needs to be read.

**Verdict path (1 round):** `bash docs/t007_oneshot_verify.sh; echo EXIT=$?` -> `EXIT=0`,
`PASS=13 FAIL=0`; then `leave_verdict(verdict="APPROVED")`.

| rule | check (mechanical, all inside that script) | result |
|---|---|---|
| 1 zero code change | per-file token multiset + line diff vs `git show HEAD:<file>` | 13/16 files 0 delta; the other 3 differ only inside t-019 (`mcp/client.rs`), t-021 (`mcp/http_tests.rs`), t-024 (`llm/client_tests.rs`) |
| 2 cleanup | rule-only banners, `file:line` pointers, commented-out statements, restatements, repeated narrative, unresolved `crate::...` refs | 0 / 0 / 0 (17 regex candidates are rustdoc bullet continuations) / 0 / 0 (legit sibling-field docs excluded) / 0 |
| 3 Swedish->English | `grep -rnP '[åäöÅÄÖ]' src/llm src/net src/mcp` | exit 1 at HEAD **and** in the work tree: no Swedish prose exists in this scope, and glossary G1-G5 of `docs/research_swedish_inventory.md` all sit outside it |
| 4 UTF-8 fixtures | every non-ASCII string literal in all 16 files compared with HEAD | byte-identical (`🔍`/`🚀` in `llm/thinking_tests.rs`; no `aao`/`e`/CJK/`wsen` bytes anywhere in scope) |
| 5 portability | `grep -rn '"/bin/' src/mcp src/llm src/net` empty; bare `cat`/`sleep`/`sh`/`tee` at `mcp/client.rs:569,605,672,693,826` and `llm/client_tests.rs:667`; `METHOD_TOOLS_CALL` = 6 uses and never re-inlined (`"tools/call"` only as raw-string SSE fixtures `mcp/http_tests.rs:59,389` plus `protocol.rs`) | PASS |
| 6 include_str! guards | 3 sites in `mcp/http_tests.rs` (405, 449, 450); split marker `#[cfg(test)]` + `mod tests {` at `mcp/client.rs:427-428`; the scanned production halves of `http.rs`/`client.rs` contain none of the needles `jsonrpc:"2.0"`, `"initialize"`, `"notifications/initialized"`, `"tools/list"`, `"tools/call"`, `MCP_PROTOCOL_VERSION`, `2024-11-05`, `AtomicU64`, `"inputSchema"`, `"isError"`; my only `client.rs` deletions are in the test half; `build_http_client_with(` still in `http.rs`, no `reqwest::Client::builder()` | PASS |
| 7 tool-name literals | `cargo test --test test_tool_name_literals` exit 0 and `test_role_gating fix_loop_source_has_no_raw_tool_name_string_literals` exit 0; BASELINE never edited by t-007; in-scope counts vs HEAD differ only by `("src/mcp/client.rs","sleep")` 0->1 inside the t-019 region, already covered by an existing BASELINE row | PASS |
| gates | fmt 0; clippy 0; `cargo test --lib -- llm net mcp --test-threads=1` 0 (113 passed; separately `llm` 42 / `net` 9 / `mcp` 63, all exit 0); full lib single-threaded 807 passed / 5 failed = exactly the sanctioned set, 0 in llm/net/mcp | PASS |

Pre-existing and deliberately untouched: 6 rustdoc *visibility* warnings (`llm/client.rs:85`,
`mcp/client.rs:5,248`, `mcp/http.rs:21,22,564` - links to private `super::protocol`,
`JsonRpcTransport`, `ChatError::terminal_outcome`) exist at HEAD too and resolve with
`--document-private-items`; they are not stale pointers.

| file | comment lines removed | added | non-comment code lines differing |
|---|---|---|---|
| src/llm/client.rs | 0 | 0 | 0 |
| src/llm/client_tests.rs | 0 | 3 | 2 |
| src/llm/mod.rs | 0 | 0 | 0 |
| src/llm/stream.rs | 0 | 0 | 0 |
| src/llm/stream_tests.rs | 5 | 0 | 0 |
| src/llm/thinking.rs | 5 | 0 | 0 |
| src/llm/thinking_tests.rs | 2 | 0 | 0 |
| src/mcp/client.rs | 5 | 29 | 21 |
| src/mcp/http.rs | 0 | 0 | 0 |
| src/mcp/http_tests.rs | 8 | 0 | 6 |
| src/mcp/mod.rs | 1 | 0 | 1 |
| src/mcp/protocol.rs | 2 | 3 | 0 |
| src/net/http.rs | 0 | 0 | 0 |
| src/net/mod.rs | 0 | 0 | 0 |
| src/net/retry.rs | 1 | 1 | 0 |
| src/net/sse.rs | 0 | 0 | 0 |

## Complete diff, 1 line of context

```
diff --git a/src/llm/client_tests.rs b/src/llm/client_tests.rs
index 27635b4..4624c7b 100644
--- a/src/llm/client_tests.rs
+++ b/src/llm/client_tests.rs
@@ -663,3 +663,6 @@ fn scripted_mcp(tools: &[&str]) -> ScriptedMcp {
         config: crate::mcp::McpServerConfig {
-            command: Some("/bin/sh".to_string()),
+            // `sh` resolved through PATH — its absolute location differs between
+            // runners (it lives in `/usr/bin` on macOS), and the `cat` plus
+            // `sleep` in the script are resolved the same way by the shell.
+            command: Some("sh".to_string()),
             args: vec![
diff --git a/src/llm/stream_tests.rs b/src/llm/stream_tests.rs
index 35bebda..590c259 100644
--- a/src/llm/stream_tests.rs
+++ b/src/llm/stream_tests.rs
@@ -146,3 +146,2 @@ async fn test_turn_stream_handler_detects_pause() {
 
-    // First chunk passes
     assert!(handler.on_chunk_with_sink("chunk1", &mut sink));
@@ -173,3 +172,2 @@ async fn test_turn_stream_handler_does_not_trigger_repetition_on_thinking() {
 
-    // Stream content
     let continue_content = handler.on_chunk_with_sink("Here is the final answer.", &mut sink);
@@ -186,3 +184,2 @@ async fn test_turn_stream_handler_enforces_thinking_budget() {
 
-    // Start thinking block
     assert!(handler.on_chunk_with_sink("<think>\n", &mut sink));
@@ -228,3 +225,2 @@ fn test_stream_config_thinking_budget_defaults() {
 
-// ---------------------------------------------------------------------------
 // Cluster net-C11 guard: the single finalize helper shared by every successful
@@ -233,3 +229,2 @@ fn test_stream_config_thinking_budget_defaults() {
 // per-terminal-site logging difference.
-// ---------------------------------------------------------------------------
 
diff --git a/src/llm/thinking.rs b/src/llm/thinking.rs
index c5deada..afefcc0 100644
--- a/src/llm/thinking.rs
+++ b/src/llm/thinking.rs
@@ -42,3 +42,2 @@ impl Default for RecoveryAdjustment {
 pub const MIN_TEMPERATURE: f32 = 0.0;
-/// See [`MIN_TEMPERATURE`].
 pub const MAX_TEMPERATURE: f32 = 2.0;
@@ -48,3 +47,2 @@ pub const MAX_TEMPERATURE: f32 = 2.0;
 pub const MIN_PENALTY: f32 = -2.0;
-/// See [`MIN_PENALTY`].
 pub const MAX_PENALTY: f32 = 2.0;
@@ -53,3 +51,2 @@ pub const MAX_PENALTY: f32 = 2.0;
 pub const MIN_TOP_P: f32 = 0.0;
-/// See [`MIN_TOP_P`].
 pub const MAX_TOP_P: f32 = 1.0;
@@ -511,5 +508,3 @@ impl NudgePolicy {
 
-// ---------------------------------------------------------------------------
 // Unit tests (Phase D checkpoint: `cargo test --lib test_llm_`)
-// ---------------------------------------------------------------------------
 
diff --git a/src/llm/thinking_tests.rs b/src/llm/thinking_tests.rs
index ad16ebd..2c9d933 100644
--- a/src/llm/thinking_tests.rs
+++ b/src/llm/thinking_tests.rs
@@ -114,3 +114,2 @@ fn test_llm_recovery_defaults_when_absent() {
 
-// ---------------------------------------------------------------------------
 // t-035d — recovery parameters must be clamped into the documented ranges.
@@ -119,3 +118,2 @@ fn test_llm_recovery_defaults_when_absent() {
 // recovery turn could never recover.
-// ---------------------------------------------------------------------------
 
diff --git a/src/mcp/client.rs b/src/mcp/client.rs
index 4fd6971..64c59ef 100644
--- a/src/mcp/client.rs
+++ b/src/mcp/client.rs
@@ -523,3 +523,2 @@ mod tests {
 
-        // Empty input list yields an empty result.
         let empty = manager.tools_for_servers(&[]);
@@ -556,6 +555,11 @@ mod tests {
 
-    /// `/bin/cat` echoes every line written to it, which is enough to drive the
-    /// `initialize` handshake and a `tools/list` round-trip through
-    /// `crate::net::pump_future` — the shared replacement for the hand-rolled
-    /// `tokio::time::timeout` around `read_line`.
+    /// `cat` — looked up on `PATH` instead of hard-coding its absolute path,
+    /// which differs between runners (`/usr/bin` on macOS) — echoes every line
+    /// written to it, which is enough to drive the `initialize` handshake and a
+    /// `tools/list` round-trip through `crate::net::pump_future` — the shared
+    /// replacement for the hand-rolled `tokio::time::timeout` around `read_line`.
+    ///
+    /// Keeps the `cfg(unix)` gate: the point of the test is the echoed byte
+    /// stream, and Windows has no equivalent that mirrors a piped stdin back out
+    /// (`more` paginates and buffers instead of echoing).
     #[cfg(unix)]
@@ -564,3 +568,3 @@ mod tests {
         let cfg = McpServerConfig {
-            command: Some("/bin/cat".to_string()),
+            command: Some("cat".to_string()),
             ..Default::default()
@@ -583,3 +587,8 @@ mod tests {
     /// The pump's idle deadline still surfaces the transport's own timeout error.
-    #[cfg(unix)]
+    ///
+    /// Made cross-platform deliberately: the only requirement is a child that
+    /// stays alive and never writes to stdout, so on Windows `cmd /C ping …`
+    /// (output sent to `NUL`) stands in for `sleep`. Program names come from
+    /// `PATH` — the previously hard-coded absolute path is missing on macOS
+    /// runners, where the binary lives in `/usr/bin` instead.
     #[tokio::test]
@@ -587,5 +596,15 @@ mod tests {
         // A "server" that never writes anything back.
+        let (command, args) = if cfg!(windows) {
+            (
+                "cmd".to_string(),
+                vec!["/C".to_string(), "ping -n 30 127.0.0.1 > NUL".to_string()],
+            )
+        } else {
+            // The sleep program is looked up through `PATH` here — no absolute
+            // path, whose location differs between Unix runners.
+            ("sleep".to_string(), vec!["30".to_string()])
+        };
         let cfg = McpServerConfig {
-            command: Some("/bin/sleep".to_string()),
-            args: vec!["30".to_string()],
+            command: Some(command),
+            args,
             ..Default::default()
@@ -638,2 +657,6 @@ mod tests {
     /// stdout stays open while the transport drives a round-trip.
+    ///
+    /// Keeps the `cfg(unix)` gate: the script is POSIX shell (`printf` +
+    /// `sleep`). `sh` is resolved through `PATH` rather than hard-coding its
+    /// absolute location, which differs between Unix runners.
     #[cfg(unix)]
@@ -648,3 +671,3 @@ mod tests {
         McpServerConfig {
-            command: Some("/bin/sh".to_string()),
+            command: Some("sh".to_string()),
             args: vec!["-c".to_string(), script],
@@ -659,2 +682,7 @@ mod tests {
     /// not two.
+    ///
+    /// Keeps the `cfg(unix)` gate: the capture needs a real `tee` (stdin mirrored
+    /// to both the capture file and the pipe back to us), and Windows ships no
+    /// such program. `tee` is resolved through `PATH`, so the test no longer
+    /// hard-codes its absolute location.
     #[cfg(unix)]
@@ -664,3 +692,3 @@ mod tests {
         let cfg = McpServerConfig {
-            command: Some("/bin/tee".to_string()),
+            command: Some("tee".to_string()),
             args: vec![capture.path().to_string_lossy().into_owned()],
@@ -790,2 +818,5 @@ mod tests {
     /// the first request id, and the handshake does not consume one.
+    ///
+    /// Keeps the `cfg(unix)` gate (the echo-backed `cat` server above): `cat` is
+    /// looked up on `PATH` rather than pinned to an absolute path.
     #[cfg(unix)]
@@ -794,3 +825,3 @@ mod tests {
         let cfg = McpServerConfig {
-            command: Some("/bin/cat".to_string()),
+            command: Some("cat".to_string()),
             ..Default::default()
diff --git a/src/mcp/http_tests.rs b/src/mcp/http_tests.rs
index 77f72dd..a3a9680 100644
--- a/src/mcp/http_tests.rs
+++ b/src/mcp/http_tests.rs
@@ -40,5 +40,3 @@ mod tests {
 
-    // ──────────────────────────────────────────────────────────────────────────
     // Protocol guards — these call production code (`crate::mcp::protocol`).
-    // ──────────────────────────────────────────────────────────────────────────
 
@@ -176,5 +174,3 @@ mod tests {
 
-    // ──────────────────────────────────────────────────────────────────────────
     // Mock MCP endpoint.
-    // ──────────────────────────────────────────────────────────────────────────
 
@@ -341,5 +337,3 @@ mod tests {
 
-    // ──────────────────────────────────────────────────────────────────────────
     // C7: both transports speak the protocol layer's envelopes on the wire.
-    // ──────────────────────────────────────────────────────────────────────────
 
@@ -389,3 +383,3 @@ mod tests {
             .await
-            .expect("tools/call");
+            .expect(METHOD_TOOLS_CALL);
 
@@ -402,6 +396,4 @@ mod tests {
 
-    // ──────────────────────────────────────────────────────────────────────────
     // C3 + C4 + C2 wiring: shared reqwest builder, shared SSE pump, shared
     // retry policy (see docs/recon_duplication_net.md).
-    // ──────────────────────────────────────────────────────────────────────────
 
@@ -507,3 +499,3 @@ mod tests {
         let server = "srv".to_string();
-        let method = "tools/call".to_string();
+        let method = METHOD_TOOLS_CALL.to_string();
         assert!(
@@ -558,3 +550,3 @@ mod tests {
         let server = "srv".to_string();
-        let method = "tools/call".to_string();
+        let method = METHOD_TOOLS_CALL.to_string();
 
diff --git a/src/mcp/mod.rs b/src/mcp/mod.rs
index e9b5aaa..4a3b817 100644
--- a/src/mcp/mod.rs
+++ b/src/mcp/mod.rs
@@ -28,4 +28,2 @@ pub use http::HttpSseConnection;
 
-// ── Name policy ────────────────────────────────────────────────────────────
-
 /// The separator used to compose an MCP server name with a raw tool name
diff --git a/src/mcp/protocol.rs b/src/mcp/protocol.rs
index a22a32a..11c4989 100644
--- a/src/mcp/protocol.rs
+++ b/src/mcp/protocol.rs
@@ -318,4 +318,5 @@ fn default_input_schema() -> Value {
 /// present but `null` stays `null` — production has always passed it through,
-/// and the C8 test copy that filtered it out was wrong (that is the drift the
-/// recon flagged at `src/mcp/http_tests.rs:110` vs `src/mcp/http.rs:304`).
+/// and the hand-copied C8 test that filtered it out was wrong (the drift is
+/// recorded on the transport-level `tools/list` decode test in `http_tests`,
+/// which exercises this function instead of a local copy).
 pub(crate) fn parse_tools(result: &Value, server_name: &str) -> Vec<McpTool> {
diff --git a/src/net/retry.rs b/src/net/retry.rs
index 4d7a96c..bff6850 100644
--- a/src/net/retry.rs
+++ b/src/net/retry.rs
@@ -12,3 +12,3 @@
 //! unification — both MCP HTTP/SSE request paths,
-//! `crate::mcp::http::HttpSseConnection::post` and
+//! `crate::mcp::http::HttpSseConnection::send_request` and
 //! `HttpSseConnection::send_notification`.
```
