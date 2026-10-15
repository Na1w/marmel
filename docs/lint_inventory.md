# Lint Inventory — authoritative `cargo fmt` / `cargo clippy` state (task t-009)

**Workspace:** `/home/fredrik/marmel/wip` (single cargo package `marmennill`, `Cargo.toml:2`)
**HEAD:** `9fab839` · **Branch state:** uncommitted change set only
**Run date (local):** 2026-10-15 02:52–03:05 +0200 (log timestamps in `marmel.log` are UTC: 00:52–01:05Z)
**Toolchain:** `cargo 1.98.0 (797e8a9bc 2026-08-05)`, `rustc 1.98.0 (88d9e12ae 2026-08-18)`, `rustfmt 1.9.0-stable (88d9e12ae1)`
**Mode:** read-only for all source/test files. No source/test/doc file other than this report was written. Only `target/` artifacts were removed (`cargo clean -p marmennill`) and scratch copies were made under `/tmp`.

---

## 0. Executive summary (the three disputed claims, settled)

| Claim | Verdict | Root cause of the disagreement |
|---|---|---|
| `clippy::needless_range_loop` at `tests/test_rebirth_system_role.rs:383` | **Was real, is now fixed. Does NOT reproduce on the current tree.** | The reporting agent linted **revision 1** of the untracked file, which contained `for index in 1..4 { assert_eq!(wire_role(&wire[index]), …) }`. That loop was rewritten to `iter().enumerate().skip(1)` at `2026-10-15T00:10:24Z` (see `marmel.log:2515`), i.e. **before** the auditor ran. Not an invocation-scope artifact, not a stale-artifact artifact. |
| `cargo fmt -- --check` diffs in `tests/test_rebirth_system_role.rs` | **TRUE — 7 hunks, 1 file.** | Only genuine formatting debt in the workspace. Introduced by the current (untracked) change set. |
| Pre-existing fmt diffs at lines **1 and 752** of `src/llm/client_tests.rs` | **FALSE — invocation artifact.** | Produced by running `rustfmt --edition 2021 --check src/llm/client_tests.rs` (verbatim at `marmel.log:2965`). Style-edition 2021 import-sorting ≠ edition 2024. Under the crate's real edition (`cargo fmt --all -- --check`, or `rustfmt --edition 2024`) that file is **clean in both HEAD and the working tree**. |
| `cargo clippy` diagnostics anywhere in the workspace | **ZERO** under every scope tried. | — |

**Total lint debt to remediate: 7 rustfmt hunks in one file. Zero clippy findings.**

---

## 1. Lint configuration (what the gates actually enforce)

`Cargo.toml` (73 lines, no `[workspace]` section → `--all` == the single package):

```
1:[package]
2:name = "marmennill"
4:edition = "2024"
5:rust-version = "1.98"
67:[lints.rust]
68:unused          = "deny"
69:dead_code       = "deny"
70:unused_imports  = "deny"
72:[lints.clippy]
73:all = { level = "deny", priority = -1 }
```

Consequences (verified live in the `-v` unit command recorded in §5.3): every clippy unit of this package is compiled with
`'--deny=clippy::all' --deny=unused_imports --deny=unused --deny=dead_code`. Therefore **any** real clippy finding in any
target (lib, bin, or any `tests/*.rs`) is a **hard compile error even without `-D warnings`**. Invocation scope cannot
explain a "warning invisible to one scope" in this repo.

**Formatting configuration:** *no* `rustfmt.toml`, `.rustfmt.toml` or `.rustfmt.json` exists anywhere in the workspace
(`ls` + `find . -maxdepth 3 -name "*rustfmt*"` → nothing, `grep` across the tree → nothing). So rustfmt runs with pure
defaults: `max_width = 100`, `fn_call_width = 60`, `tab_spaces = 4`, `reorder_imports = true`, **style edition derived from
`edition = "2024"`**. The `fn_call_width = 60` default is what makes hunks 4/6/7 (lines 263 / 288 / 317 / 319, each 80–81
columns) break vertically while visually identical 40-column calls stay on one line — this is expected default behaviour,
not a config mismatch.

**CI gates that must pass** (`.github/workflows/ci.yml`):
- line 35 and line 78: `cargo fmt --all -- --check`
- line 38 and line 81: `cargo clippy --all-targets --all-features -- -D warnings`
- line 186: `cargo clippy --all-targets --all-features --target <matrix.target> -- -D warnings`

---

## 2. Clean-build-state procedure actually executed (in this order)

```
$ cargo clean -p marmennill
Removed 2958 files, 5.2 GiB total          # 1st clean (full package wipe)
$ cargo fmt --all -- --check               # -> exit 1, 88 lines, 7 hunks (see §3 + Appendix A)
$ cargo clean -p marmennill
Removed 311 files, 260.6 MiB total         # 2nd clean, immediately before the authoritative clippy run
$ cargo clippy --all-targets --all-features -- -D warnings
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 21.16s      # exit code 0, 0 diagnostics
```

Dependency artifacts were reused from `target/` (only `marmennill`'s own check/rmeta/fingerprint artifacts were wiped),
so every `marmennill` target — lib, bin and all 28 integration tests — was re-linted from scratch in both runs.

---

## 3. Formatting inventory — `cargo fmt --all -- --check`

Exit code **1**. Output touches **exactly one file** and **7 rustfmt hunks** (Appendix A holds it verbatim).

| File | Hunks | rustfmt hunk headers | Lines changed (current revision) | Class |
|---|---|---|---|---|
| `tests/test_rebirth_system_role.rs` | **7** | `:169`, `:184`, `:205`, `:260`, `:267`, `:285`, `:314` | 172–176 · 187 · 208–211 · 263 · 267–271 · 288 · 317, 319 | **(A)** introduced by the current change set (file is untracked) |
| every other file in the crate (lib, bin `marmel`, 27 other `tests/*.rs`, `tests/common/mod.rs`) | 0 | — | — | clean (nothing to classify) |

Per-hunk detail (all in `tests/test_rebirth_system_role.rs`):

| # | Header | Current lines | What rustfmt wants |
|---|---|---|---|
| 1 | `:169` | 172–176 | collapse `assert_eq!(\n requests.len(),\n 1,\n "exactly one request body is inspected"\n );` → one line |
| 2 | `:184` | 187 | split `msg["role"].as_str().expect("every wire message carries a role")` into a 3-line method chain |
| 3 | `:205` | 208–211 | collapse `assert!(matches!(\n engine.messages()[2],\n Message::System { .. }\n ));` → one line |
| 4 | `:260` | 263 | expand `assert_eq!(end_before, 3, "the checkpoint at [2] extends the pinned window");` → 4 lines |
| 5 | `:267` | 267–271 | collapse `assert_eq!(wire_role(&wire[2]), "user", "the wire role is demoted ...");` → one line |
| 6 | `:285` | 288 | expand `assert!(is_injected_advisory(content), "still recognizable as injected");` → 4 lines |
| 7 | `:314` | 317 **and** 319 | expand both `assert_eq!(wire_content(&wire[N]).as_bytes(), LEGACY_CHECKPOINT_K.as_bytes());` → 4 lines each |

Verified mechanically on a `/tmp` copy (workspace file untouched):

```
$ cp tests/test_rebirth_system_role.rs /tmp/tb.rs && rustfmt --edition 2024 /tmp/tb.rs
$ diff -u tests/test_rebirth_system_role.rs /tmp/tb.rs
6 unified-diff hunks: @@ -169,11 @@ @@ -184,7 @@ @@ -205,10 @@ @@ -260,15 @@ @@ -285,7 @@ @@ -314,9 @@
19 lines removed, 22 lines added; file 405 -> 408 lines
```
(6 unified hunks vs 7 rustfmt hunks because rustfmt prints `:260` and `:267` separately while a unified diff merges them.)

**No pre-existing fmt debt exists anywhere in HEAD.** Evidence:
1. `git show HEAD:src/llm/client_tests.rs | rustfmt --edition 2024 --check` → exit 0, **0 hunks**.
2. `git show HEAD:src/llm/client.rs` (+ its `#[path = "client_tests.rs"] mod tests;` child) → exit 0, **0 hunks**.
3. Working-tree check (§3) flags only the one untracked file, so all 141 other tracked `.rs` files are clean.
4. `tests/common/mod.rs` (tracked, not reachable from any test target — a file `cargo fmt` can silently skip) was checked
   directly: `rustfmt --edition 2024 --check tests/common/mod.rs` → exit 0, **0 hunks**.

---

## 4. Clippy inventory — `cargo clippy --all-targets --all-features -- -D warnings`

Run from a freshly `cargo clean -p marmennill` state. **Exit code 0. Zero diagnostics.**

| File | Line | Lint | Message | Class |
|---|---|---|---|---|
| — | — | — | *no clippy diagnostic exists in this workspace under any checked scope* | n/a |

Full verbatim output (Appendix B.1):

```
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 21.16s
```

---

## 5. Scope cross-check + proof that the disputed target is really linted (step 5)

### 5.1 Scope matrix (all four invocations, same working tree)

| Invocation | Re-checked from scratch? | Result |
|---|---|---|
| `cargo clippy --all-targets --all-features -- -D warnings` | yes (after `cargo clean -p marmennill`) | **0 diagnostics**, exit 0, 21.16 s |
| `cargo clippy --lib --tests` | yes ("Checking marmennill", 20.32 s) | **0 diagnostics** (Appendix B.2) |
| `cargo clippy --test test_rebirth_system_role` | no — served from cache created by the `--all-targets` run (0.28 s) | clean (Appendix B.3) |
| `cargo clippy --lib --tests -v -- -D warnings --cfg marmel_lint_probe` | yes (novel flag set forces full re-lint; `--cfg` probe is inert — no source depends on it) | **0 diagnostics**, 17.07 s, 30 units, incl. the disputed target (Appendix B.4) |

**Conclusion:** the scope is *not* the differentiator. `--all-targets` ⊇ `--lib --tests` ⊇ `--test test_rebirth_system_role`
(the verbose run shows `--lib --tests` selects 30 units: `marmennill` lib ×2, bin `marmel`, and all 28 `test_*` integration
targets). A finding in `tests/test_rebirth_system_role.rs` would be reported by every one of them, and would be a hard
error even without `-D warnings` because `Cargo.toml:72-73` denies `clippy::all`.

### 5.2 File facts for the disputed file (step 4)

```
$ git status --porcelain | grep rebirth
?? tests/test_rebirth_system_role.rs              # UNTRACKED -> no git history, cannot be diffed against HEAD
$ wc -l tests/test_rebirth_system_role.rs
405
$ stat -c '%n size=%s mtime=%y' tests/test_rebirth_system_role.rs
tests/test_rebirth_system_role.rs size=17200 mtime=2026-10-15 02:12:41.658761535 +0200
$ md5sum tests/test_rebirth_system_role.rs
c805b7649cdb4be48306b3dd8b42e139
```

### 5.3 Loop inventory of the disputed file (exhaustive)

```
$ grep -n "for \|\.iter()\|range(\|\.len()" tests/test_rebirth_system_role.rs
246:        .iter()            (systems: iter().enumerate().filter().map().collect())
324:    for index in [2usize, 7] {                              # array iteration, NOT a range
392:    for (index, msg) in wire.iter().enumerate().skip(1) {    # iterator+enumerate, both bindings used
173/217/274/314/368/374/386/402:  .len() used in assertions
```

* There is **no** `for i in 0..xs.len()` / `for i in a..b` index-range loop in the file. `needless_range_loop` only fires
  on index-range loops that index a slice/array, so it **cannot** fire on this revision.
* Line **383** of the current revision is `let second_backend = sse_backend().await;` — not a loop
  (context `:380-395` printed in Appendix C).
* The target really is linted with the package lint table, verbatim from the forced verbose run:
  `... clippy-driver .../rustc --crate-name test_rebirth_system_role --edition=2024 tests/test_rebirth_system_role.rs
  '--deny=clippy::all' --deny=unused_imports --deny=unused --deny=dead_code --test ...`

### 5.4 Adjudication of the disputed `needless_range_loop` — **timeline evidence**

The workspace log `.marmel/marmel.log` records the whole life of this untracked file:

| Time (UTC) | Log line | Event |
|---|---|---|
| `2026-10-15T00:08:00.041Z` | `marmel.log:2415` | `write_file` creates `tests/test_rebirth_system_role.rs` (396-line revision 1). **Revision 1 contains** `for index in 1..4 { assert_eq!(wire_role(&wire[index]), "user", "index {index}"); }` |
| *(between 00:08 and 00:10)* | — | implementer runs clippy → **real denial**, verbatim in `coder-t-005` session log: `error: the loop variable \`index\` is used to index \`wire\` --> tests/test_rebirth_system_role.rs:383:18 / 383 \| for index in 1..4 { … help: consider using an iterator and enumerate()` |
| `2026-10-15T00:10:24.975Z` | `marmel.log:2515` | `replace` on the same file: `old_str: "    for index in 1..4 {\n        assert_eq!(wire_role(&wire[index]), …"` → `new_str: "    for (index, msg) in wire.iter().enumerate().skip(1) {\n        assert_eq!(wire_role(msg), …"` — **the lint is fixed here**, exactly as clippy suggested |
| `2026-10-15T02:12:41` local (`00:12:41Z`) | `stat` | last modification of the file (current 405-line revision, md5 `c805b76…`) |
| `2026-10-15T00:23–00:44Z` | auditor (t-007) | `cargo clean -p marmennill` + `clippy --lib`, `--lib --tests`, `--test test_rebirth_system_role` → all clean |
| `2026-10-15T00:52–01:05Z` | this inventory (t-009) | two `cargo clean -p marmennill` + all four scopes → all clean, incl. `--all-targets --all-features -- -D warnings` |

**Definitive ruling:** *both* earlier reports were correct at their own moments. The `needless_range_loop` denial was a
genuine, correctly-transcribed diagnostic against **revision 1** of the untracked test file (line 383 of that revision);
it was fixed in-tree at 00:10:24Z, two minutes after the file was created. Every report made after that edit — including
the auditor's — is also correct: the current tree has **no such lint**. The disagreement is explained by
**untracked-file revision drift**, not by `--all-targets` vs `--lib --tests` vs `--test <name>`, and not by a stale or
contaminated build (the auditor and this run both re-linted after a package wipe). No clippy follow-up is required.

**Corollary for t-011:** there is nothing to fix. t-011 should be executed as a verification-only task.

### 5.5 Adjudication of the disputed `src/llm/client_tests.rs` fmt hunks (lines 1 and 752)

The earlier agent's command is recorded verbatim at `marmel.log:2965`:

```
rustfmt --edition 2021 --check src/llm/client_tests.rs 2>&1 | grep "^Diff in"
```

Reproduced here (same working tree):

```
$ rustfmt --edition 2021 --check src/llm/client_tests.rs | grep "^Diff in"
Diff in /home/fredrik/marmel/wip/src/llm/client_tests.rs:1:
Diff in /home/fredrik/marmel/wip/src/llm/client_tests.rs:752:
$ rustfmt --edition 2024 --check src/llm/client_tests.rs | grep -c "^Diff in"
0
```

Both hunks are pure **import-ordering** churn caused by rustfmt style-edition 2021 vs the crate's edition 2024
(2021 sorts `use crate::net::{BACKOFF_BASE_MS, MAX_ATTEMPTS, Retryable}` as `{Retryable, BACKOFF_BASE_MS, MAX_ATTEMPTS}`
and re-sorts the `MAX_PENALTY / MAX_TEMPERATURE / … / apply_recovery_report` list inside
`test_llm_recovery_request_body_stays_in_documented_ranges`, which starts at line 752). They are **not** defects:

* HEAD revision: `git show HEAD:src/llm/client_tests.rs` → 0 hunks under `--edition 2024`.
* Working-tree revision (1063 lines, mtime `2026-10-15 02:47:22`): 0 hunks under `--edition 2024` and 0 hunks under
  `cargo fmt --all -- --check`.

**Ruling:** the claim is **false as a lint issue**; it is a wrong-invocation artifact (`--edition 2021`, and the crate's
own CI gate is `cargo fmt --all`). Classification: no finding, therefore neither (A) nor (B).

---

## 6. Final classification table (step 6)

| # | Kind | Location | Detail | Class |
|---|---|---|---|---|
| F1–F7 | rustfmt | `tests/test_rebirth_system_role.rs` hunk headers 169 / 184 / 205 / 260 / 267 / 285 / 314 (source lines 172–176, 187, 208–211, 263, 267–271, 288, 317, 319) | 7 hunks, −19 / +22 lines, 405 → 408 | **(A)** introduced by the current change set — the file is untracked (`?? tests/test_rebirth_system_role.rs`), so it cannot exist in HEAD |
| C1– | clippy | — | zero diagnostics under `--all-targets --all-features -- -D warnings`, `--lib --tests`, `--test test_rebirth_system_role`, and a forced re-lint | n/a |
| D1 | stale claim | `src/llm/client_tests.rs:1`, `:752` | edition-2021 rustfmt artifact, not a real diff at HEAD or in the tree | n/a (no finding) |
| D2 | stale claim | `tests/test_rebirth_system_role.rs:383` | `needless_range_loop` was real on revision 1, fixed at 00:10:24Z | **(A)** — was introduced by the change set and **already remediated in-tree** |

Pre-existing (class B) lint debt in HEAD: **none found**.

---

## 7. Recommended remediation plan (for t-010 / t-011 / t-012)

### 7.1 Exactly what to run

```bash
cd /home/fredrik/marmel/wip

# t-010 — the ONLY formatting action needed. One command, no scoping tricks required:
cargo fmt --all
cargo fmt --all -- --check            # must now exit 0 with empty output
git diff --stat                       # expect NO tracked-file churn (the formatted file is untracked)

# t-011 — nothing to fix; verify instead (expect exit 0, zero diagnostics):
cargo clean -p marmennill
cargo clippy --all-targets --all-features -- -D warnings

# t-012 — final gate parity with CI:
cargo fmt --all -- --check
cargo clippy --all-targets --all-features -- -D warnings
cargo build --all-targets --all-features
cargo test
```

### 7.2 Files needing manual (non-mechanical) fixes

**None.** All 7 hunks are mechanical rustfmt output inside one untracked test file; running `cargo fmt --all` resolves
the entire lint debt. Do **not** hand-edit them (a hand edit will likely diverge from rustfmt's
`fn_call_width = 60` decisions and fail the gate again). No `#[allow]` of any kind is warranted, and none is needed.

Behaviour-preservation note for the next agent: `cargo fmt --all` touches only whitespace/line breaks, so the 4
integration tests' semantics and the `src/llm/client.rs` demotion hunk are unaffected; `cargo test --test
test_rebirth_system_role` should stay 4/4 green (it was 4/4 at the current revision).

### 7.3 ⚠️ The "enormous mechanical diff" warning — and where it actually comes from

There is **no** unrelated pre-existing file that legitimately needs formatting today; the only real debt is the 7 hunks in
`tests/test_rebirth_system_role.rs`. The enormous-diff risk is created **by the tool invocation, not by the tree**:

* **Never run bare `rustfmt` / `rustfmt --edition 2015`** on these files — it dies with hard parse errors, e.g.
  `error[E0670]: 'async fn' is not permitted in Rust 2015` (measured on `src/llm/client_tests.rs`) and
  `error: let chains are only allowed in Rust 2024 or later` (`src/agents/catalog.rs:285`, `:323`). An agent that pipes
  such output through `grep "^Diff in"` sees *no* diffs and may wrongly conclude rustfmt is broken.
* **Never run `rustfmt --edition 2021 …`** (the mistake behind dispute #3). Measured blast radius on this tree:
  **125 spurious hunks across 46 of 142 tracked `.rs` files** — pure import-order rewriting in files nobody touched
  (worst offenders: `src/mcp/mod.rs` 11, `src/agents/runner/mod.rs` 9, `src/markers.rs` 7,
  `src/agents/runner/fix_loop.rs` 6, `src/ui/tui/tests.rs` 5, `src/mcp/http_tests.rs` 5). That is the "huge mechanical
  diff in unrelated pre-existing files" to avoid, and it is entirely self-inflicted.
* Always format through **`cargo fmt`** (reads `edition = "2024"` from `Cargo.toml` and follows `#[path]` module
  declarations). If a per-file invocation is ever unavoidable, use
  `rustfmt --edition 2024 <file>` and note that `client.rs`'s `#[path = "client_tests.rs"] mod tests;` child module
  resolution requires the sibling file to be present.
* If the next agent wants a scoped command, `cargo fmt -p marmennill` is equivalent here (single-package workspace);
  `cargo fmt --all` is what CI runs, so prefer it for gate parity.
* Housekeeping: `.marmel.old*/tmp/**` and `.marmel.old*/.marmel/**` contain stale `.rs` backups that are **not** part of any
  cargo target, so `cargo fmt` will never touch them — do not "fix" them, and do not add them to the repo.

### 7.4 Acceptance criterion for the final gate

`cargo fmt --all -- --check` exits 0 with **no output** (no exclusion list needed — nothing is legitimately exempt), and
`cargo clippy --all-targets --all-features -- -D warnings` exits 0.

---

## Appendix A — verbatim `cargo fmt --all -- --check` output (ANSI colours stripped; 88 lines total)

```
Diff in /home/fredrik/marmel/wip/tests/test_rebirth_system_role.rs:169:
         .received_requests()
         .await
         .expect("wiremock exposes the requests it received");
-    assert_eq!(
-        requests.len(),
-        1,
-        "exactly one request body is inspected"
-    );
+    assert_eq!(requests.len(), 1, "exactly one request body is inspected");
     let body: serde_json::Value =
         serde_json::from_slice(&requests[0].body).expect("the request body is JSON");
     body["messages"]

Diff in /home/fredrik/marmel/wip/tests/test_rebirth_system_role.rs:184:
 
 /// The `role` tag of one captured wire message.
 fn wire_role(msg: &serde_json::Value) -> &str {
-    msg["role"].as_str().expect("every wire message carries a role")
+    msg["role"]
+        .as_str()
+        .expect("every wire message carries a role")
 }
 
 /// The `content` string of one captured wire message.

Diff in /home/fredrik/marmel/wip/tests/test_rebirth_system_role.rs:205:
     let engine = restored_engine(legacy_restored_records());
     // The fixture really is the legacy spelling — otherwise this test would assert
     // nothing about the shape it names.
-    assert!(matches!(
-        engine.messages()[2],
-        Message::System { .. }
-    ));
+    assert!(matches!(engine.messages()[2], Message::System { .. }));
     assert_eq!(engine.messages()[2].role(), "system");
 
     let backend = sse_backend().await;

Diff in /home/fredrik/marmel/wip/tests/test_rebirth_system_role.rs:260:
 async fn test_restored_legacy_system_checkpoint_stays_detectable_in_memory() {
     let engine = restored_engine(legacy_restored_records());
     let end_before = engine.pinned_prefix_end();
-    assert_eq!(end_before, 3, "the checkpoint at [2] extends the pinned window");
+    assert_eq!(
+        end_before, 3,
+        "the checkpoint at [2] extends the pinned window"
+    );
 
     let backend = sse_backend().await;
     let wire = wire_messages(&backend, &request_for(&engine)).await;

Diff in /home/fredrik/marmel/wip/tests/test_rebirth_system_role.rs:267:
-    assert_eq!(
-        wire_role(&wire[2]),
-        "user",
-        "the wire role is demoted ..."
-    );
+    assert_eq!(wire_role(&wire[2]), "user", "the wire role is demoted ...");
 
     // ... and the in-memory history is untouched by that demotion.
     assert_eq!(engine.messages().len(), 7, "the send never mutates history");

Diff in /home/fredrik/marmel/wip/tests/test_rebirth_system_role.rs:285:
         "the REBIRTH_CHECKPOINT_PREFIX detector still matches"
     );
     assert_eq!(content.as_bytes(), LEGACY_CHECKPOINT_1.as_bytes());
-    assert!(is_injected_advisory(content), "still recognizable as injected");
+    assert!(
+        is_injected_advisory(content),
+        "still recognizable as injected"
+    );
     assert_eq!(
         engine.pinned_prefix_end(),
         end_before,

Diff in /home/fredrik/marmel/wip/tests/test_rebirth_system_role.rs:314:
     assert_eq!(wire.len(), 8);
     assert_eq!(wire_role(&wire[0]), "system");
     assert_eq!(wire_role(&wire[2]), "user");
-    assert_eq!(wire_content(&wire[2]).as_bytes(), LEGACY_CHECKPOINT_1.as_bytes());
+    assert_eq!(
+        wire_content(&wire[2]).as_bytes(),
+        LEGACY_CHECKPOINT_1.as_bytes()
+    );
     assert_eq!(wire_role(&wire[7]), "user");
-    assert_eq!(wire_content(&wire[7]).as_bytes(), LEGACY_CHECKPOINT_2.as_bytes());
+    assert_eq!(
+        wire_content(&wire[7]).as_bytes(),
+        LEGACY_CHECKPOINT_2.as_bytes()
+    );
     assert_eq!(wire_role(&wire[5]), "assistant");
     assert_eq!(wire_role(&wire[6]), "tool");
```

## Appendix B — verbatim clippy outputs

### B.1 `cargo clippy --all-targets --all-features -- -D warnings` (after `cargo clean -p marmennill`)
```
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 21.16s
```
exit code **0** · diagnostics: **0**

### B.2 `cargo clippy --lib --tests`
```
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 20.32s
```
diagnostics: **0**

### B.3 `cargo clippy --test test_rebirth_system_role`
```
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 0.28s
```
(cached-clean; the fingerprint was produced by the clean `--all-targets` run in B.1) · diagnostics: **0**

### B.4 forced full re-lint of lib+tests with verbose unit listing
```
$ cargo clippy --lib --tests -v -- -D warnings --cfg marmel_lint_probe
     Running `.../clippy-driver .../rustc --crate-name marmennill --edition=2024 src/lib.rs ... '--deny=clippy::all' ...`
     Running `.../clippy-driver .../rustc --crate-name marmel --edition=2024 src/main.rs ...`
     Running `.../clippy-driver .../rustc --crate-name test_rebirth_system_role --edition=2024 tests/test_rebirth_system_role.rs
              ... '--deny=clippy::all' --deny=unused_imports --deny=unused --deny=dead_code --test ...`
     ... 27 more test_* units ...
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 17.07s
```
units linted: 30 · diagnostics: **0**

### B.5 historical (superseded) diagnostic — quoted from the `coder-t-005` session log, for the record
```
    Checking marmennill v1.0.0 (/home/fredrik/marmel/wip)
error: the loop variable `index` is used to index `wire`
   --> tests/test_rebirth_system_role.rs:383:18
    |
383 |     for index in 1..4 {
    |                  ^^^^
    = note: `-D clippy::needless-range-loop` implied by `-D clippy::all`
help: consider using an iterator and enumerate()
    |
383 -     for index in 1..4 {
383 +     for (index, <item>) in wire.iter().enumerate().take(4).skip(1) {

error: could not compile `marmennill` (test "test_rebirth_system_role") due to 1 previous error
```
This is the origin of the disputed claim. It corresponds to revision 1 of the file (pre-00:10:24Z) and no longer applies.

## Appendix C — disputed-file context (current revision, `tests/test_rebirth_system_role.rs`)

```
380	    );
381	    assert_eq!(msgs[checkpoint_index].role(), "user");
382	
383	    let second_backend = sse_backend().await;          # <-- disputed line 383: NOT a loop
384	    let wire = wire_messages(&second_backend, &request_for(&engine)).await;
385	
386	    assert_eq!(wire.len(), 4, "the collapsed history is sent as-is");
387	    assert_eq!(
388	        wire_role(&wire[0]),
389	        "system",
390	        "the leading system prompt must never be demoted"
391	    );
392	    for (index, msg) in wire.iter().enumerate().skip(1) {   # <-- clippy's own suggested rewrite
393	        assert_eq!(wire_role(msg), "user", "index {index}");
394	    }
```

Other loops in the file: `:246` `.iter().enumerate().filter(...).map(...).collect()`, `:324 for index in [2usize, 7] {`
(array literal, not a range → `needless_range_loop` cannot fire).

## Appendix D — integrity of this run

`git status --porcelain` is byte-identical before and after this inventory
(` M src/llm/client.rs`, ` M src/llm/client_tests.rs`, `?? tests/test_rebirth_system_role.rs`, plus the pre-existing
untracked docs/scratch entries). The three change-set files are unchanged:
`tests/test_rebirth_system_role.rs` 405 lines / md5 `c805b7649cdb4be48306b3dd8b42e139` / mtime `2026-10-15 02:12:41`;
`src/llm/client_tests.rs` 1063 lines / mtime `02:47:22`; `src/llm/client.rs` 1114 lines / mtime `02:19:16`.
`git diff --stat` still reports `src/llm/client.rs | 26 +++++-`, `src/llm/client_tests.rs | 223 +++…+++`,
`2 files changed, 248 insertions(+), 1 deletion(-)`. Only `target/` was deleted/rebuilt, and `/tmp` scratch copies
(`/tmp/head_client_tests.rs`, `/tmp/head_client.rs`, `/tmp/tb.rs`) were created for HEAD comparisons.
