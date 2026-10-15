#!/usr/bin/env bash
# =============================================================================
# scripts/cross_platform_check.sh — local cross-platform parity gate (Linux host)
#
# Implements the gate designed and measured in docs/cross_platform_gating.md §A.
#
# The target list is NOT hard-coded in this script: it is read from
# config/gate_targets.txt, the single list shared with CI (§D "keep one shared list
# in the repo … so adding a target is a single edit").  Do NOT iterate "every
# installed target": the bare-metal (thumb*) and wasm32 legs cannot build this crate,
# and x86_64-pc-windows-msvc is proven red locally because onig_sys/aws-lc-sys need an
# MSVC-targeting C toolchain (§3.2).  Hence the FIXED allow-list.
#
# Legs, in §A order:
#   host:fmt       cargo fmt --all -- --check                                [ci job `lint`]
#   host:clippy    cargo clippy --all-targets --all-features -- -D warnings  [ci job `lint`]
#   host:test      cargo test --all-targets --all-features --no-fail-fast
#                    -- --test-threads=1 --skip <6 env/state-blocked tests:
#                       4 PTY-device-blocked + 1 PTY-path-blocked + 1 session-state-sensitive>
#   host:doc-test  cargo test --doc --all-features
#   cross:check    cargo check  --all-targets --all-features --target T
#   cross:clippy   cargo clippy --all-targets --target T -- -D warnings
#
# Why BOTH cross legs (§3.1, measured): `cargo clippy … --target T -- -D warnings` did
# NOT replay the four src/harness/sandbox.rs rustc diagnostics that plain `cargo check`
# emitted for the same target, while clippy added the src/config.rs lint that check
# never sees.  The two legs are not interchangeable.
#
# Why --test-threads=1 plus 6 exact skips (§3.4, measured exit 0 / 954 passed).  The 6
# skips are NOT one category — "PTY-dependent" is accurate for only 5 of them, and the
# 6th is a repo-state sensitivity (docs/fix_verification.md §A/§B/§C, coverage concern C1):
#   * PTY-DEVICE-BLOCKED (4): harness::pty::tests::test_harness_pty_process_group_kill,
#     test_interactive_pty_manager_lifecycle, test_run_command_parses_timeout_seconds_argument,
#     test_run_command_pty_cancelled_by_worker_token — /dev/pts/ptmx is mode 000 in this
#     sandbox, so openpty() fails with EACCES (fixtures at src/harness/pty_tests.rs:7, 92,
#     107, 164).  Purely an environment/device hole.
#   * PTY-PATH-BLOCKED (1): test_prompt_based_tool_gating (tests/test_role_gating.rs:296)
#     reaches the same openpty EACCES through the real dispatch path
#     (dispatch_for(run_command …) -> pty::run_command); it reproduces identically on a
#     pristine tree, so it is device-blocked, not a defect.
#   * STATE-SENSITIVE / FLAKY (1), NOT PTY: harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable
#     (src/harness/mod.rs:1251) fails at src/harness/mod.rs:1261 if and only if a `.marmel/`
#     session dir exists in CWD (2x2 tree/presence matrix, docs/fix_verification.md §B).
#     CI never has such a dir, so the failure mode is invisible there — the skip is a
#     workaround for a dirty working tree, NOT a PTY classification; it needs its own ticket.
#   * FLAKY CASCADING FAILURES — deliberately NOT skipped: the parallel-mode failures
#     (harness::tool_resolution_tests::*, poisoned LazyLock over the process-global MCP
#     registry) are removed by the serial run; skip alone does not remove them.
# The skip names are SUBSTRING matches, so the exact test paths are named instead of
# `harness::pty::` (which would silently filter 30 tests, 25 of them passing and relevant).
#
# Fail-closed rule (§A): every triple listed `local`/`both` in config/gate_targets.txt
# must have its rust-std installed, otherwise the gate exits non-zero — unless the
# caller explicitly tolerates that one specific triple with
#     --allow-missing-target <triple>      (repeatable)
# so "green" never means "not checked".  The final summary names every tolerated gap
# and the CI job that has to carry it.
#
# Exit codes:
#   0  every executed leg passed and no local|both coverage hole is unaccounted for
#   1  a leg failed, a local|both rust-std is missing without an explicit tolerance,
#      or config/gate_targets.txt is missing/unparseable
#   2  usage error
# =============================================================================
set -euo pipefail

# --- self-location: usable from any CWD ------------------------------------------
script_src=${BASH_SOURCE[0]:-$0}
script_dir=$(cd -P -- "$(dirname -- "$script_src")" >/dev/null && pwd)
repo_root=$(cd -P -- "$script_dir/.." >/dev/null && pwd)
target_list=$repo_root/config/gate_targets.txt

# Skip list: 6 names, but THREE different reasons (labels matter — reviewers must be
# able to tell an environment hole from a real code smell).  Evidence:
# docs/fix_verification.md §A/§B/§C and coverage concern C1.
#   PTY-DEVICE-BLOCKED — openpty() EACCES, /dev/pts/ptmx is mode 000 on this box:
#       --skip harness::pty::tests::test_harness_pty_process_group_kill
#       --skip harness::pty::tests::test_interactive_pty_manager_lifecycle
#       --skip harness::pty::tests::test_run_command_parses_timeout_seconds_argument
#       --skip harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
#   PTY-PATH-BLOCKED — same openpty failure reached via dispatch_for(run_command …)
#   -> pty::run_command (tests/test_role_gating.rs:296); device-blocked, not a defect:
#       --skip test_prompt_based_tool_gating
#   STATE-SENSITIVE / FLAKY — NOT PTY: fails at src/harness/mod.rs:1261 iff a `.marmel/`
#   session dir exists in CWD (src/harness/mod.rs:1251); passes in a clean tree, so CI
#   never sees it and this skip hides a working-tree/state problem, not a PTY one:
#       --skip harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable
readonly SKIPS=(
    --skip harness::pty::tests::test_harness_pty_process_group_kill
    --skip harness::pty::tests::test_interactive_pty_manager_lifecycle
    --skip harness::pty::tests::test_run_command_parses_timeout_seconds_argument
    --skip harness::pty::tests::test_run_command_pty_cancelled_by_worker_token
    --skip harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable
    --skip test_prompt_based_tool_gating
)

usage() {
    cat <<'USAGE'
scripts/cross_platform_check.sh — local cross-platform parity gate (Linux host)

USAGE:
    scripts/cross_platform_check.sh [OPTIONS]

Runs, from the repository root (resolved from $0, so any CWD works):
    1. host legs    cargo fmt --all -- --check
                    cargo clippy --all-targets --all-features -- -D warnings
                    cargo test --all-targets --all-features --no-fail-fast
                        -- --test-threads=1 <6 documented --skip names>
                    cargo test --doc --all-features
    2. cross legs   for every <triple> with scope local|both in config/gate_targets.txt
                    cargo check  --all-targets --all-features --target T
                    cargo clippy --all-targets --target T -- -D warnings

OPTIONS:
    -h, --help                Show this help and exit 0.
    --only-host               Run the host legs only. Cross-target legs are skipped
                              entirely and the fail-closed missing-std accounting does
                              not apply — the summary states loudly that cross-target
                              parity was NOT exercised, so use this for iteration speed
                              only, never as the gate of record. Upstream, the windows-gnu
                              cross leg is carried by CI job `cross-check` /
                              "Cross-target check (windows-gnu)", gated by job `lint`.
    --allow-missing-target <triple>
    --allow-missing-target=<triple>
                              Tolerate exactly ONE triple that config/gate_targets.txt
                              lists as local|both but whose rust-std is not installed
                              here. Repeatable. Without it the gate fails closed.
                              Today's real case:
                                  --allow-missing-target aarch64-apple-darwin
                              (rustup cannot install any new target on this box:
                              ~/.rustup is read-only, docs/cross_platform_gating.md §3.3;
                              CI job `test` / Test (macos-latest) is load-bearing, and
                              x86_64-pc-windows-gnu is carried by CI job `cross-check` /
                              "Cross-target check (windows-gnu)", gated by job `lint`.)

EXIT CODES:
    0  all executed legs green, no unaccounted coverage hole
    1  a leg failed / a local|both rust-std is missing without tolerance / bad target list
    2  usage error
USAGE
}

die() {
    printf 'FATAL: %s\n' "$*" >&2
    exit 1
}

# --- argument parsing ------------------------------------------------------------
only_host=0
allowed_missing=()

is_allowed_missing() {
    local want=$1 entry
    for entry in ${allowed_missing[@]+"${allowed_missing[@]}"}; do
        [ "$entry" = "$want" ] && return 0
    done
    return 1
}

used_allow=()
mark_allow_used() {
    local want=$1 entry
    for entry in ${used_allow[@]+"${used_allow[@]}"}; do
        [ "$entry" = "$want" ] && return 0
    done
    used_allow+=("$want")
}

is_used_allow() {
    local want=$1 entry
    for entry in ${used_allow[@]+"${used_allow[@]}"}; do
        [ "$entry" = "$want" ] && return 0
    done
    return 1
}

while [ $# -gt 0 ]; do
    case $1 in
        -h|--help)
            usage
            exit 0
            ;;
        --only-host)
            only_host=1
            ;;
        --allow-missing-target)
            if [ $# -lt 2 ] || [ -z "${2:-}" ]; then
                printf 'USAGE ERROR: --allow-missing-target needs a target triple\n' >&2
                exit 2
            fi
            allowed_missing+=("$2")
            shift
            ;;
        --allow-missing-target=*)
            allowed_missing+=("${1#--allow-missing-target=}")
            ;;
        *)
            printf 'USAGE ERROR: unknown option: %s\n\n' "$1" >&2
            usage >&2
            exit 2
            ;;
    esac
    shift
done

# --- preflight -------------------------------------------------------------------
cd "$repo_root"
command -v cargo >/dev/null 2>&1 || die "cargo not found on PATH"

host_target=$(rustc -vV 2>/dev/null | sed -n 's/^host: //p')
[ -n "$host_target" ] || die "could not determine host target triple from 'rustc -vV'"

installed_targets=''
if command -v rustup >/dev/null 2>&1; then
    installed_targets=$(rustup target list --installed 2>/dev/null || true)
else
    printf 'WARN: rustup not found — every cross target will be reported as not installed\n' >&2
fi

is_installed_target() {
    local want=$1 entry
    [ -n "$installed_targets" ] || return 1
    while IFS= read -r entry; do
        [ "$entry" = "$want" ] && return 0
    done <<<"$installed_targets"
    return 1
}

# CI job that carries the coverage the local box cannot provide. Job ids and display
# names are taken from .github/workflows/ci.yml, which has exactly four jobs:
# `lint` = "Lint & Format", `test` = "Test (${{ matrix.os }})" (Test (ubuntu-latest) /
# Test (macos-latest) / Test (windows-latest)), `cross-check` = "Cross-target check
# (windows-gnu)" (needs: lint) and `release-build` = "Release Build (…)".
ci_owner() {
    case $1 in
        x86_64-unknown-linux-gnu)
            printf "CI job 'test' — Test (ubuntu-latest), plus job 'lint' — Lint & Format"
            ;;
        aarch64-apple-darwin)
            printf "CI job 'test' — Test (macos-latest) [native], job 'release-build' — Release Build (macos-latest)"
            ;;
        x86_64-pc-windows-msvc)
            printf "CI job 'test' — Test (windows-latest) [native], job 'release-build' — Release Build (windows-latest)"
            ;;
        x86_64-pc-windows-gnu)
            printf "CI job 'cross-check' — Cross-target check (windows-gnu), gated by 'lint' (steps 'Run Clippy for the cross target' + 'Check all targets for the cross target', triple resolved from config/gate_targets.txt)"
            ;;
        *)
            printf "no CI job maps this triple — treat as an uncovered cfg class"
            ;;
    esac
}

# --- leg bookkeeping -------------------------------------------------------------
rows=()
failures=()
tolerated=()
passed=0
failed=0
skipped=0

add_row() {
    rows+=("$(printf '%-30s %-26s %-26s %8s' "$1" "$2" "$3" "$4")")
}

run_leg() {
    # $1 = leg id, $2 = target label, rest = the command to execute
    local id=$1 label=$2 rc=0 started=0 finished=0 elapsed=0
    shift 2
    printf '\n────── LEG %s [%s] ──────────────────────────────────────────────\n' "$id" "$label"
    printf '$ %s\n' "$*"
    started=$(date +%s)
    if "$@"; then
        rc=0
    else
        rc=$?
    fi
    finished=$(date +%s)
    elapsed=$((finished - started))
    if [ "$rc" -eq 0 ]; then
        passed=$((passed + 1))
        add_row "$id" "$label" "PASS" "${elapsed}s"
        printf '── LEG %s: PASS (%ss)\n' "$id" "$elapsed"
    else
        failed=$((failed + 1))
        add_row "$id" "$label" "FAIL (exit $rc)" "${elapsed}s"
        printf '── LEG %s: FAIL (exit %s, %ss)\n' "$id" "$rc" "$elapsed"
        failures+=("leg $id [$label] exited $rc")
    fi
}

# --- parse the shared target list -------------------------------------------------
[ -f "$target_list" ] || die "target list not found: $target_list"

cross_targets=()
ci_only_targets=()
line_no=0
while IFS= read -r line || [ -n "$line" ]; do
    line_no=$((line_no + 1))
    line=${line%$'\r'}
    line=${line%%#*}
    read -r triple scope extra <<<"$line"
    [ -n "${triple:-}" ] || continue
    if [ -z "${scope:-}" ] || [ -n "${extra:-}" ]; then
        die "malformed line $line_no in $target_list: expected '<triple> <local|ci|both>', got: $line"
    fi
    case $triple in
        *[!A-Za-z0-9._-]*) die "invalid target triple on line $line_no of $target_list: $triple" ;;
    esac
    case $scope in
        local | both)
            cross_targets+=("$triple")
            ;;
        ci)
            ci_only_targets+=("$triple")
            ;;
        *)
            die "unknown scope '$scope' on line $line_no of $target_list (expected local|ci|both)"
            ;;
    esac
done <"$target_list"

[ ${#cross_targets[@]} -gt 0 ] || die "no local|both target found in $target_list"

# --- coverage pre-audit: loud, BEFORE any leg runs -------------------------------
# Same fail-closed accounting as the cross loop, announced up front so the caller
# learns immediately that the run is going to be red; the legs still execute so the
# table shows exactly what was verified.
preaudit_missing=()
for target in "${cross_targets[@]}"; do
    if [ "$target" = "$host_target" ]; then
        continue
    fi
    if ! is_installed_target "$target"; then
        if is_allowed_missing "$target"; then
            printf 'COVERAGE: %s rust-std is not installed — explicitly tolerated via --allow-missing-target.\n' "$target" >&2
        else
            preaudit_missing+=("$target")
        fi
    fi
done
if [ "$only_host" -eq 0 ] && [ ${#preaudit_missing[@]} -gt 0 ]; then
    printf '\n*** GATE WILL FAIL CLOSED (not "green"): %s target(s) listed local|both in %s\n' \
        "${#preaudit_missing[@]}" "$target_list" >&2
    printf '*** have NO rust-std installed, so their cfg class is NOT verified locally:\n' >&2
    for t in "${preaudit_missing[@]}"; do
        printf '***      %s  -> coverage owner today: %s\n' "$t" "$(ci_owner "$t")" >&2
    done
    printf '***   Fix it:  rustup target add <triple>\n' >&2
    printf '***   Or state the gap explicitly (repeatable, one triple per flag):\n' >&2
    printf '***      %s --allow-missing-target <triple>\n' "$0" >&2
    printf '\n' >&2
fi

# --- 1+2: host legs (identical to CI job `lint`, plus serialized tests) -----------
printf 'repo root   : %s\n' "$repo_root"
printf 'host triple : %s\n' "$host_target"
printf 'target list : %s\n' "$target_list"
printf 'local scope : %s\n' "${cross_targets[*]}"
printf 'ci-only     : %s\n' "${ci_only_targets[*]:-<none>}"

run_leg "host:fmt" "$host_target" cargo fmt --all -- --check
run_leg "host:clippy" "$host_target" cargo clippy --all-targets --all-features -- -D warnings
run_leg "host:test" "$host_target" cargo test --all-targets --all-features --no-fail-fast -- \
    --test-threads=1 "${SKIPS[@]}"
run_leg "host:doc-test" "$host_target" cargo test --doc --all-features

# --- 3: cross-target legs (check AND clippy per target, §A) ----------------------
missing_now=()

if [ "$only_host" -eq 1 ]; then
    printf '\nNOTE: --only-host — cross-target legs were NOT run; cross-target parity is unverified.\n' >&2
else
    for target in "${cross_targets[@]}"; do
        if [ "$target" = "$host_target" ]; then
            skipped=$((skipped + 1))
            add_row "cross:*" "$target" "HOST-LEG (already covered)" "-"
            tolerated+=("$target is the host triple — covered by the host legs (fmt, clippy --all-targets --all-features -D warnings, full test suite, doc tests) and by $(ci_owner "$target").")
            continue
        fi
        if ! is_installed_target "$target"; then
            if is_allowed_missing "$target"; then
                mark_allow_used "$target"
                skipped=$((skipped + 1))
                add_row "cross:check+clippy" "$target" "SKIPPED (std missing)" "-"
                tolerated+=("$target not installable locally — rust-std not installed (rustup cannot install any target on this box: ~/.rustup is read-only, docs/cross_platform_gating.md §3.3) — tolerated only because --allow-missing-target $target was passed — covered by $(ci_owner "$target").")
                printf '\nWARN: target %s not installed — its cfg class is NOT checked locally.\n      Tolerated because --allow-missing-target %s was passed.\n      Coverage responsibility handed to: %s\n' \
                    "$target" "$target" "$(ci_owner "$target")" >&2
            else
                failed=$((failed + 1))
                missing_now+=("$target")
                add_row "cross:check+clippy" "$target" "FAIL (rust-std missing)" "-"
                failures+=("target $target is listed local|both in $target_list but its rust-std is not installed — cfg class unverified")
            fi
            continue
        fi
        run_leg "cross:check" "$target" cargo check --all-targets --all-features --target "$target"
        run_leg "cross:clippy" "$target" cargo clippy --all-targets --all-features --target "$target" -- -D warnings
    done

    for target in ${ci_only_targets[@]+"${ci_only_targets[@]}"}; do
        skipped=$((skipped + 1))
        add_row "cross:*" "$target" "NOT-CHECKED (scope=ci)" "-"
        tolerated+=("$target scope=ci by design — cross-compiling it from Linux is proven red (onig_sys + aws-lc-sys build scripts, docs/cross_platform_gating.md §3.2) — covered by $(ci_owner "$target").")
    done
fi

# --- report -----------------------------------------------------------------------
printf '\n\n======================= PER-LEG PASS/FAIL TABLE =======================\n'
printf '%-30s %-26s %-26s %8s\n' LEG TARGET RESULT TIME
printf '%s\n' '-------------------------------------------------------------------------------------------'
for row in ${rows[@]+"${rows[@]}"}; do
    printf '%s\n' "$row"
done

printf '\n============================ SUMMARY ================================\n'
printf 'legs executed : %s  (PASS %s / FAIL %s)   skipped-by-design rows: %s\n' \
    "$((passed + failed))" "$passed" "$failed" "$skipped"

if [ ${#failures[@]} -gt 0 ]; then
    printf '\nFAILURES:\n'
    for f in "${failures[@]}"; do
        printf '  x %s\n' "$f"
    done
fi

printf '\nCOVERAGE GAPS / TOLERATED GAPS (what this run did NOT verify locally):\n'
if [ "$only_host" -eq 1 ]; then
    printf '  ! --only-host was passed: NO cross-target leg ran. Windows and Apple cfg\n'
    printf '    classes are entirely unverified in this run. Use without --only-host, or\n'
    printf '    rely on the CI jobs (lint / test / cross-check — Cross-target check\n'
    printf '    (windows-gnu), gated by lint).\n'
fi
if [ ${#missing_now[@]} -gt 0 ]; then
    for t in "${missing_now[@]}"; do
        printf '  ! %s: rust-std NOT installed and NOT tolerated -> gate fails closed.\n' "$t"
        printf '    To state the gap explicitly: --allow-missing-target %s\n' "$t"
        printf '    Owner of that coverage instead: %s\n' "$(ci_owner "$t")"
    done
fi
if [ ${#tolerated[@]} -gt 0 ]; then
    for g in "${tolerated[@]}"; do
        printf '  - %s\n' "$g"
    done
fi
printf '  - %s tests are skipped locally, split across three reasons (docs/fix_verification.md\n' "$(( ${#SKIPS[@]} / 2 ))"
printf '    §A/§B/§C): 4 are PTY-device-blocked (/dev/pts/ptmx is mode 000 here, so openpty\n'
printf '    fails EACCES) and test_prompt_based_tool_gating is PTY-path-blocked (its\n'
printf '    dispatch_for(run_command …) reaches the same pty::run_command call); the 6th,\n'
printf '    test_harness_delegate_task_manager_routes_and_returns_deliverable, is NOT PTY-dependent\n'
printf '    but state-sensitive/flaky — it fails at src/harness/mod.rs:1261 only when a .marmel/\n'
printf '    session dir exists in the working tree. The 5 PTY-blocked ones run natively in the\n'
printf '    CI job '\''test'\'' legs: Test (ubuntu-latest), Test (macos-latest), Test (windows-latest).\n'
printf '  - check/clippy do not link and no target binary is executed: behavioural parity\n'
printf '    (PTY, crossterm/console, landlock, process groups, path separators) remains\n'
printf '    CI-only (docs/cross_platform_gating.md §3.4, Residual risk 1–3).\n'

# An --allow-missing-target that tolerated nothing is a caller mistake (typo, or the std
# got installed meanwhile) worth surfacing — but not a reason to fail a green gate.
for t in ${allowed_missing[@]+"${allowed_missing[@]}"}; do
    if ! is_used_allow "$t"; then
        printf '\nNOTE: --allow-missing-target %s had no effect: its rust-std is installed,\n' "$t"
        printf '      or it is not listed local|both in %s.\n' "$target_list"
    fi
done

printf '\nVERDICT: '
if [ "$failed" -eq 0 ]; then
    printf 'PASS — %s leg(s) green, %s coverage gap row(s) explicitly accounted for above.\n' \
        "$passed" "$skipped"
    exit 0
fi
printf 'FAIL — %s leg(s) red.\n' "$failed"
if [ ${#missing_now[@]} -gt 0 ]; then
    printf 'Gate fails closed: %s target(s) listed local|both in %s have no rust-std.\n' \
        "${#missing_now[@]}" "$target_list"
    printf 'Re-run with --allow-missing-target <triple> for each deliberately missing target\n'
    printf '(so the tolerated gap is on the record), or fix coverage with: rustup target add %s\n' "${missing_now[0]}"
fi
exit 1
