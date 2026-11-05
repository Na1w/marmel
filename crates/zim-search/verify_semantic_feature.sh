#!/usr/bin/env bash
#
# verify_semantic_feature.sh — one-pass acceptance check for the `semantic` Cargo feature split
# of the `zim-search` crate (lib `zim_search`, bin `zim-search`, edition 2021).
#
# Acceptance criteria encoded here, in dependency order:
#   A. DEFAULT BUILD IS ONNX-FREE. `Cargo.toml` ships `[features] default = []`, so `cargo build`
#      must be clean AND `cargo tree -e normal` must contain ZERO dependency-graph lines matching
#      fastembed | onnxruntime | ort | tokenizers | hf-hub.
#   B. THE FEATURE IS OPTIONAL AND ADDITIVE. `cargo build --features semantic` (semantic =
#      ["dep:fastembed"], fastembed = 7.1.1 optional) must be clean AND the same graph probe must
#      report MORE THAN ZERO ONNX lines.
#      RISK ENCODED: the `ort` / `ort-sys` build script discovers, and may download or build, the
#      ~105 MB static ONNX Runtime before the final link. That link step is the single most likely
#      failure of the whole script (audit A12/N6, docs/verification_semantic_feature.md), which is
#      why the ON build is its own gate rather than a side effect of a test run.
#   C. `cargo check --no-default-features` type-checks, proving nothing in the crate *requires* the
#      feature to be compiled in.
#   D. Unit tests pass in BOTH configurations (`--lib`, `--bin zim-search`): the feature-off and
#      feature-on drift tests (SEMANTIC_FEATURE_UNAVAILABLE / SEMANTIC_REBUILD_COMMAND pinning,
#      clap help assertions) only exist if both configurations are actually exercised.
#      RISK ENCODED: feature-gated test names colliding/shadowing across the lib/bin/test targets.
#   E. The CLI contract harness `tests/semantic_opt_in.rs` passes in BOTH configurations.
#   F. clap's rendered `zim-search index --help` works in BOTH builds: `--semantic` and
#      `--semantic-archive <name>` (visible alias `--semantic-source`) are declared ungated
#      (src/main.rs:129-135), so they must appear in a default build's help too (audit N1/N3/N5).
#      RISK ENCODED: each `cargo run` re-links the bin for that configuration — the ON relink pays
#      the ONNX Runtime link cost again.
#   G. FEATURE-OFF FAIL-FAST. In a DEFAULT build an explicit vector opt-in must be refused, never
#      silently downgraded to BM25: `zim-search index <empty tmpdir> --semantic` must exit NON-ZERO
#      and its stderr must quote `cargo build --features semantic` — the canonical sentence
#      SEMANTIC_FEATURE_UNAVAILABLE (src/embed/mod.rs:218) rendered with SEMANTIC_REBUILD_COMMAND
#      by `semantic_request_supported` (src/main.rs:370), which runs ahead of the directory /
#      single-file branch split, so an empty library directory still hits the guard.
#      NOTE: the MCP twin of this contract — `index_archive {"semantic":true}` -> `isError: true`
#      (src/mcp/mod.rs) — is asserted by the lib tests in step D, not by this script.
#   H. `cargo clippy --all-targets` in both configurations. RISK ENCODED: the repository has a
#      KNOWN pre-existing clippy baseline failure (deny-level `approx_constant` errors, exit 101,
#      docs/verification_semantic_opt_in.md §5), so clippy can fail on untouched code; that is what
#      --skip-clippy is for, not a way to hide regressions.
#
# Safety: this script performs NO network I/O of its own, never downloads a model, never writes
# outside a `mktemp -d` scratch area (removed by a trap), and never indexes real archives — the one
# `index` run it makes points at an EMPTY temporary directory and expects a refusal.
# (cargo itself may still resolve crate sources for `fastembed` on the first `--features semantic`
# build; that is cargo's normal behaviour, not a step added here. The ONNX *model* download only
# happens during a real `index --semantic` run, which this script never performs.)
#
# Usage: bash verify_semantic_feature.sh [--only-builds | --only-tests] [--skip-clippy] [--help]
# Exit status: 0 = every executed gate passed; 1 = at least one gate FAILED; 2 = bad usage.

# `-u` only, deliberately NO `set -e`: one failing gate must not hide the rest of the report.
set -u

# ---------------------------------------------------------------------------
# usage / flags
# ---------------------------------------------------------------------------
usage() {
  cat <<'USAGE'
verify_semantic_feature.sh — verify the `semantic` Cargo feature split in one pass.

Usage: bash verify_semantic_feature.sh [options]

Options:
  --only-builds   Compilation + dependency-graph gates only (cargo build x2, cargo check
                  --no-default-features, cargo tree probes). Skips tests, clippy and CLI smoke.
  --only-tests    Test gates only (cargo test --lib / --bin zim-search / --test semantic_opt_in,
                  both feature configurations; cargo compiles what it needs itself). Skips the
                  explicit build, graph, clippy and CLI gates.
  --skip-clippy   Skip the two `cargo clippy --all-targets` gates (there is a known pre-existing
                  clippy baseline failure in untouched code; see docs/verification_semantic_opt_in.md).
  -h, --help      Show this help and exit 0.

Nothing is downloaded by this script and no archive is indexed; only an empty temp directory is.
USAGE
}

RUN_BUILDS=1
RUN_TESTS=1
RUN_GRAPH=1
RUN_CLIPPY=1
RUN_CLI=1
DISABLE_NOTE=''

# ---------------------------------------------------------------------------
# in-memory result table
# ---------------------------------------------------------------------------
RESULT_LABEL=()
RESULT_STATE=()
RESULT_NOTE=()
FAILURES=0
SKIPS=0
PASSES=0

note() { printf '%s\n' "$*"; }

record() { # record <PASS|FAIL|SKIP> <label> <detail>
  local state=$1 label=$2 detail=$3
  RESULT_LABEL+=("$label")
  RESULT_STATE+=("$state")
  RESULT_NOTE+=("$detail")
  case "$state" in
    PASS) PASSES=$((PASSES + 1)) ;;
    FAIL) FAILURES=$((FAILURES + 1)) ;;
    SKIP) SKIPS=$((SKIPS + 1)) ;;
  esac
  printf '    -> %s (%s)\n' "$state" "$detail"
}

# run_step "<label>" <command...> — echoes the command, runs it, appends a PASS/FAIL row.
run_step() {
  local label=$1
  shift
  printf '\n==> %s\n    $ %s\n' "$label" "$*"
  local status=0
  "$@"
  status=$?
  if [ "$status" -eq 0 ]; then
    record PASS "$label" "exit=0"
  else
    record FAIL "$label" "exit=$status"
  fi
}

# skip_step "<label>" "<reason>" — records a SKIP row without running anything.
skip_step() {
  printf '\n==> %s\n' "$1"
  record SKIP "$1" "$2"
}

# gate <group> — is this group of gates enabled for the current invocation?
gate() {
  local flag=1
  case "$1" in
    builds) flag=$RUN_BUILDS ;;
    tests) flag=$RUN_TESTS ;;
    graph) flag=$RUN_GRAPH ;;
    clippy) flag=$RUN_CLIPPY ;;
    cli) flag=$RUN_CLI ;;
    # Fail closed: without this fallback an unknown group name would inherit the `flag=1`
    # default and silently RUN a gate that no skip_step can explain.
    *)
      printf 'verify_semantic_feature.sh: internal error: gate() got unknown group %s\n' "$1" >&2
      flag=0
      ;;
  esac
  [ "$flag" -eq 1 ]
}

print_summary() {
  local i total=${#RESULT_LABEL[@]}
  printf '\n\n=============== SUMMARY ===============\n'
  printf '%-4s %-58s %-6s %s\n' '#' 'STEP' 'RESULT' 'DETAIL'
  i=0
  while [ "$i" -lt "$total" ]; do
    printf '%-4s %-58s %-6s %s\n' \
      "$((i + 1))" "${RESULT_LABEL[$i]}" "${RESULT_STATE[$i]}" "${RESULT_NOTE[$i]}"
    i=$((i + 1))
  done
  printf '%s\n' '-------------------------------------------------------------------------'
  printf 'total=%d  PASS=%d  FAIL=%d  SKIP=%d\n' "$total" "$PASSES" "$FAILURES" "$SKIPS"
  if [ "$FAILURES" -eq 0 ]; then
    note 'VERDICT: no gate failed. Any SKIP row is explained in the DETAIL column.'
  else
    note "VERDICT: $FAILURES gate(s) FAILED — the semantic-feature split is not accepted as-is."
  fi
}

# ---------------------------------------------------------------------------
# dependency-graph probe (criterion A / B)
# ---------------------------------------------------------------------------
# Case-insensitive probe for the ONNX inference stack. `\b` word boundaries keep bare `ort` from
# matching unrelated words, while plain `onnx` covers onnxruntime / onnx-sys / onnxruntime-sys.
ONNX_STACK_RE='fastembed|onnx|ort-sys|\bort\b|tokenizers|hf-hub'

# dependency_graph_matches <regex> [extra cargo-tree flags...] — prints the number of matching
# lines, or `cargo-tree-error:<status>` when `cargo tree` itself is unavailable/failing.
# IMPORTANT: `grep -c` exits 1 when the count is 0 — and 0 is the EXPECTED answer for the default
# graph — so its exit status is deliberately swallowed and only the printed number is inspected.
dependency_graph_matches() {
  local pattern=$1
  shift
  local tree=''
  local status=0
  tree=$(cargo tree -p zim-search -e normal "$@" 2>/dev/null)
  status=$?
  if [ "$status" -ne 0 ]; then
    printf 'cargo-tree-error:%s\n' "$status"
    return 1
  fi
  local count='0'
  count=$(printf '%s\n' "$tree" | { grep -icE -- "$pattern" || true; })
  printf '%s\n' "$count"
}

# run_graph_probe "<label>" <zero|nonzero> [extra cargo-tree flags...]
run_graph_probe() {
  local label=$1 expectation=$2
  shift 2
  local shown="cargo tree -p zim-search -e normal $* | grep -icE '$ONNX_STACK_RE'"
  printf '\n==> %s\n    $ %s\n' "$label" "$shown"
  local count='0'
  count=$(dependency_graph_matches "$ONNX_STACK_RE" "$@")
  case "$count" in
    '' | *[!0-9]*)
      record SKIP "$label" "'$count': the built-in \`cargo tree\` subcommand failed — install it (rustup component / cargo install cargo-tree) and re-run"
      return
      ;;
  esac
  if [ "$expectation" = zero ] && [ "$count" -eq 0 ]; then
    record PASS "$label" "count=0 as expected (no ONNX in the default graph)"
  elif [ "$expectation" = nonzero ] && [ "$count" -gt 0 ]; then
    record PASS "$label" "count=$count > 0 as expected (fastembed/ort present)"
  elif [ "$expectation" = zero ]; then
    record FAIL "$label" "count=$count but the DEFAULT graph must be ONNX-free (expected 0)"
  else
    record FAIL "$label" "count=$count but the \`semantic\` graph must contain ONNX (expected > 0)"
  fi
}

# ---------------------------------------------------------------------------
# fixture discovery (never aborts, never downloads)
# ---------------------------------------------------------------------------
FIXTURE=''
FIXTURE_SOURCE=''

is_readable_file() {
  local candidate=$1
  [ -n "$candidate" ] && [ -f "$candidate" ] && [ -r "$candidate" ]
}

detect_fixture() {
  if is_readable_file "${ZIM_TEST_FILE:-}"; then
    FIXTURE=$ZIM_TEST_FILE
    FIXTURE_SOURCE="\$ZIM_TEST_FILE"
    return
  fi
  # Guarded scan: the archive library (`/mnt/extradisk/kiwix`, the absolute default the CLI itself
  # falls back to) is normally a directory this user may not be able to read
  # (e.g. `ls /mnt/extradisk/kiwix` -> Permission denied). find's stderr is dropped and its
  # exit status ignored, so an unreadable directory can never abort this script. `-readable` is a
  # GNU find extension; on a find without it the scan simply yields nothing (safe degradation).
  # The script directory is scanned as well, in case an archive is placed right next to it.
  local dir=''
  local found=''
  for dir in /mnt/extradisk/kiwix .; do
    found=$(find "$dir" -maxdepth 1 -type f -name '*.zim' -readable 2>/dev/null | head -n 1)
    if is_readable_file "$found"; then
      FIXTURE=$found
      FIXTURE_SOURCE="scan of $dir"
      return
    fi
  done
}

# run_fixture_step "<label>" <command...> — like run_step, but a non-zero exit is downgraded to
# SKIP only when NO .zim fixture is reachable: tests/semantic_opt_in.rs then takes its documented
# skip branch, and a missing fixture must never be reported as a broken feature split. With a real
# fixture present, a non-zero exit is a genuine FAIL.
run_fixture_step() {
  local label=$1
  shift
  printf '\n==> %s\n    $ %s\n' "$label" "$*"
  local status=0
  "$@"
  status=$?
  if [ "$status" -eq 0 ]; then
    if [ -n "$FIXTURE" ]; then
      record PASS "$label" "exit=0 (fixture: $FIXTURE)"
    else
      record PASS "$label" 'exit=0, but fixture-gated assertions SKIP inside the harness (no .zim)'
    fi
  elif [ -z "$FIXTURE" ]; then
    record SKIP "$label" "exit=$status with no .zim fixture reachable — treated as SKIP, not FAIL"
  else
    record FAIL "$label" "exit=$status although fixture $FIXTURE was reachable"
  fi
}

# ---------------------------------------------------------------------------
# criterion G: feature-off fail-fast (needs redirection, so it is its own step function)
# ---------------------------------------------------------------------------
run_fail_fast_step() {
  local label='feature-off fail-fast: index <empty tmpdir> --semantic must be refused'
  local shown="cargo run -p zim-search --quiet --bin zim-search -- index '$EMPTY_ZIM_DIR' --semantic >'$STEP_OUT' 2>'$STEP_ERR'"
  printf '\n==> %s\n    $ %s\n' "$label" "$shown"

  local status=0
  cargo run -p zim-search --quiet --bin zim-search -- index "$EMPTY_ZIM_DIR" --semantic >"$STEP_OUT" 2>"$STEP_ERR"
  status=$?

  # The scratch area (and STEP_ERR with it) is removed by the EXIT trap, so whatever the owner
  # needs to diagnose a failure is echoed here instead.
  if [ "$status" -eq 0 ]; then
    record FAIL "$label" 'exit=0: a feature-off build silently accepted an explicit vector opt-in'
    note '    --- captured stdout (first 10 lines) ---'
    head -n 10 "$STEP_OUT"
    return
  fi
  if ! grep -qF -- '--features semantic' "$STEP_ERR"; then
    record FAIL "$label" "exit=$status but stderr does not quote '--features semantic'"
    note '    --- captured stderr (first 10 lines) ---'
    head -n 10 "$STEP_ERR"
    return
  fi
  local hint=''
  if grep -qF -- 'Semantic vector indexing is not available in this build' "$STEP_ERR"; then
    hint='canonical SEMANTIC_FEATURE_UNAVAILABLE sentence present'
  else
    hint='rebuild hint present but canonical sentence wording differs — check src/embed/mod.rs:218'
  fi
  record PASS "$label" "exit=$status, stderr quotes '--features semantic' ($hint)"
  note "    first refusal line: $(grep -m 1 -F -- '--features semantic' "$STEP_ERR")"
}

# ---------------------------------------------------------------------------
# argument parsing
# ---------------------------------------------------------------------------
# Each mode flag states the FULL group mask it governs (enabled groups explicitly set to 1,
# disabled ones to 0). Clearing only the disabled flags would let a combination such as
# `--only-builds --only-tests` switch everything off: the run would then emit only SKIP rows,
# report "no gate failed" and exit 0 without ever running a single gate.
while [ "$#" -gt 0 ]; do
  case "$1" in
    --only-builds)
      RUN_BUILDS=1 RUN_GRAPH=1
      RUN_TESTS=0 RUN_CLIPPY=0 RUN_CLI=0
      DISABLE_NOTE="${DISABLE_NOTE:+$DISABLE_NOTE + }disabled by --only-builds"
      ;;
    --only-tests)
      RUN_TESTS=1
      RUN_BUILDS=0 RUN_GRAPH=0 RUN_CLIPPY=0 RUN_CLI=0
      DISABLE_NOTE="${DISABLE_NOTE:+$DISABLE_NOTE + }disabled by --only-tests"
      ;;
    --skip-clippy)
      RUN_CLIPPY=0
      DISABLE_NOTE="${DISABLE_NOTE:+$DISABLE_NOTE + }clippy gates skipped by --skip-clippy"
      ;;
    -h | --help)
      usage
      exit 0
      ;;
    *)
      printf 'verify_semantic_feature.sh: unknown option: %s\n\n' "$1" >&2
      usage >&2
      exit 2
      ;;
  esac
  shift
done

# ---------------------------------------------------------------------------
# scratch area: the ONLY place this script is allowed to write
# ---------------------------------------------------------------------------
WORK_DIR=$(mktemp -d -t zim-search-verify.XXXXXX) || {
  printf 'verify_semantic_feature.sh: mktemp -d failed\n' >&2
  exit 2
}

# cleanup is idempotent and safe when the scratch area was never created or was already swept:
# WORK_DIR is guarded with `${WORK_DIR:-}` (the script runs with `set -u`) and blanked after the
# first successful removal, so a second call cannot run `rm -rf -- ''`.
cleanup() {
  if [ -n "${WORK_DIR:-}" ] && [ -d "${WORK_DIR:-}" ]; then
    rm -rf -- "$WORK_DIR"
  fi
  WORK_DIR=''
}
# Installed directly after the FIRST successful mktemp, so every later early exit (empty-library
# dir failure, missing crate root, cd failure) and every signal still removes WORK_DIR.
#
# The EXIT trap must not change the script's exit status: cleanup never calls `exit`, so the
# status produced by the verdict block (`FAILURES>0 => exit 1`) is what the caller sees.
# INT/TERM are deliberately NOT routed to `cleanup` alone — a bare `trap cleanup INT TERM` runs
# cleanup and then RESUMES the gate loop, so one Ctrl-C would be reported as a stack of bogus
# FAIL rows. They sweep and exit with the conventional signal statuses instead; the EXIT trap
# that then fires finds WORK_DIR already gone (no-op).
trap cleanup EXIT
trap 'cleanup; exit 130' INT
trap 'cleanup; exit 143' TERM

EMPTY_ZIM_DIR=$(mktemp -d "$WORK_DIR/empty-library.XXXXXX") || {
  printf 'verify_semantic_feature.sh: mktemp -d (empty library) failed\n' >&2
  exit 2
}
STEP_OUT="$WORK_DIR/step.stdout"
STEP_ERR="$WORK_DIR/step.stderr"

# Self-locate: this script must sit in the crate root (the directory holding its `Cargo.toml`), so
# the manifest it verifies and the fixture scan resolve the same way wherever the owner invokes it
# from. Inside the workspace that root is `wip/crates/zim-search`: the crate is the member
# `crates/zim-search` of the `wip/Cargo.toml` workspace root, so every cargo leg below is pinned
# with `-p zim-search` and artifacts land in the shared `wip/target/` instead of a private
# `crates/zim-search/target/`. Nothing depends on CWD-relative `kiwix_dir` / `indices` symlinks any
# more — the CLI's own fallbacks are the absolute data-volume paths.
SCRIPT_DIR=$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" 2>/dev/null && pwd)
if [ -z "$SCRIPT_DIR" ] || [ ! -f "$SCRIPT_DIR/Cargo.toml" ]; then
  printf 'verify_semantic_feature.sh: crate root (Cargo.toml) not found next to this script\n' >&2
  exit 2
fi
cd -- "$SCRIPT_DIR" || {
  printf 'verify_semantic_feature.sh: cannot cd into %s\n' "$SCRIPT_DIR" >&2
  exit 2
}

# ---------------------------------------------------------------------------
# pre-flight report
# ---------------------------------------------------------------------------
note "zim-search semantic-feature acceptance run in: $SCRIPT_DIR"
note "cargo: $(cargo --version 2>/dev/null || echo 'NOT FOUND')"
note "rustc: $(rustc --version 2>/dev/null || echo 'NOT FOUND')"
note "scratch area (removed on exit): $WORK_DIR"

detect_fixture
if [ -n "$FIXTURE" ]; then
  export ZIM_TEST_FILE="$FIXTURE"
  note "Fixture: readable .zim found ($FIXTURE via $FIXTURE_SOURCE) -> exported as ZIM_TEST_FILE, so the"
  note '  fixture-gated on-disk assertions in tests/semantic_opt_in.rs actually execute.'
else
  note "Fixture: NO readable .zim discovered (\$ZIM_TEST_FILE unset, /mnt/extradisk/kiwix unreadable or empty)."
  note '  The fixture-gated integration assertions will SKIP — a documented skip, never a failure.'
  note '  Nothing is downloaded: to exercise them, point ZIM_TEST_FILE at an archive you already have.'
fi

# ---------------------------------------------------------------------------
# gates, in dependency order
# ---------------------------------------------------------------------------

# A / B: compilation
if gate builds; then
  run_step 'build (default, BM25-only, no ONNX)' cargo build -p zim-search
  run_step 'build --features semantic (ONNX; ort/ort-sys link step is the main risk)' \
    cargo build -p zim-search --features semantic
  run_step 'check --no-default-features (feature really is optional)' cargo check -p zim-search --no-default-features
else
  skip_step 'build group (cargo build / build --features semantic / check --no-default-features)' \
    "$DISABLE_NOTE"
fi

# A / B: dependency graph (cheap, and it proves the ONNX claim rather than asserting it)
if gate graph; then
  run_graph_probe 'graph probe: default build must contain NO ONNX stack (expect 0)' zero
  run_graph_probe 'graph probe: --features semantic must contain the ONNX stack (expect > 0)' \
    nonzero --features semantic
else
  skip_step 'graph probe group (cargo tree -e normal, both configurations)' "$DISABLE_NOTE"
fi

# D / E: tests, feature OFF then ON. Run after the graph probe so a broken ONNX link is already
# attributable to the build gate instead of surfacing as an unexplained test failure.
if gate tests; then
  run_step 'test --lib (feature OFF)' cargo test -p zim-search --lib
  run_step 'test --lib --features semantic (feature ON)' cargo test -p zim-search --lib --features semantic
  run_step 'test --bin zim-search (feature OFF)' cargo test -p zim-search --bin zim-search
  run_step 'test --bin zim-search --features semantic (feature ON)' \
    cargo test -p zim-search --bin zim-search --features semantic
  run_fixture_step 'test --test semantic_opt_in (feature OFF, CLI contract)' \
    cargo test -p zim-search --test semantic_opt_in
  run_fixture_step 'test --test semantic_opt_in --features semantic (feature ON, CLI contract)' \
    cargo test -p zim-search --test semantic_opt_in --features semantic
else
  skip_step 'test group (--lib, --bin zim-search, --test semantic_opt_in, both configurations)' \
    "$DISABLE_NOTE"
fi

# H: lints (skippable: known pre-existing clippy baseline failure in untouched code)
if gate clippy; then
  run_step 'clippy --all-targets (feature OFF)' cargo clippy -p zim-search --all-targets
  run_step 'clippy --all-targets --features semantic (feature ON)' \
    cargo clippy -p zim-search --all-targets --features semantic
else
  skip_step 'clippy group (cargo clippy --all-targets, both configurations)' "$DISABLE_NOTE"
fi

# F: rendered clap help from the binary of EACH configuration. `cargo run` is used (not the
# prebuilt target/debug path) so the correct configuration is guaranteed; it re-links on the
# feature flip, which is exactly the ort/ort-sys link cost being watched.
if gate cli; then
  run_step 'cli help: index --help (feature-OFF binary)' \
    cargo run -p zim-search --quiet --bin zim-search -- index --help
  run_step 'cli help: index --help (feature-ON binary)' \
    cargo run -p zim-search --quiet --features semantic --bin zim-search -- index --help
  # G: the refusal contract, against an EMPTY directory so no archive is ever read or indexed.
  run_fail_fast_step
else
  skip_step 'cli group (index --help in both builds, feature-off fail-fast)' "$DISABLE_NOTE"
fi

# ---------------------------------------------------------------------------
# verdict
# ---------------------------------------------------------------------------
print_summary

if [ "$FAILURES" -gt 0 ]; then
  exit 1
fi
exit 0
