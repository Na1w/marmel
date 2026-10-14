#!/usr/bin/env bash
# t-007 one-shot verification (comment cleanup + Swedish->English in src/llm, src/net, src/mcp).
# Purpose: let an auditor confirm the whole t-007 contract in a single round.
# Usage:   bash docs/t007_oneshot_verify.sh          (run from the repo root)
# Exit:    0 when every check passes.
set -u
scope="src/llm src/net src/mcp"
pass=0; fail=0
chk(){ if [ "$1" = "$2" ]; then echo "PASS  $3 (got $1)"; pass=$((pass+1)); else echo "FAIL  $3 (got $1, want $2)"; fail=$((fail+1)); fi; }

# --- mandated gates -------------------------------------------------------
cargo fmt --all -- --check >/tmp/t7_fmt.out 2>&1;                                  chk $? 0 "cargo fmt --all -- --check"
cargo clippy --all-targets --all-features -- -D warnings >/tmp/t7_clippy.out 2>&1; chk $? 0 "cargo clippy --all-targets --all-features -- -D warnings"
cargo test --lib -- llm net mcp --test-threads=1 >/tmp/t7_test.out 2>&1;           chk $? 0 "cargo test --lib -- llm net mcp --test-threads=1"
cargo test --test test_tool_name_literals >/tmp/t7_lit.out 2>&1;                   chk $? 0 "cargo test --test test_tool_name_literals (rule 7)"
cargo test --test test_role_gating fix_loop_source_has_no_raw_tool_name_string_literals >/tmp/t7_rg.out 2>&1; chk $? 0 "cargo test --test test_role_gating fix_loop_source_has_no_raw_tool_name_string_literals (rule 7)"
# full lib suite, single-threaded: only the documented environmental failures may fail
cargo test --lib -- --test-threads=1 >/tmp/t7_full.out 2>&1
unsanctioned=$(awk '/^failures:$/{f=1;next} /^test result/{f=0} f' /tmp/t7_full.out \
  | grep -E '^ +[a-z_:]+$' | sort -u \
  | grep -vE '^ +(harness::pty::tests::(test_harness_pty_process_group_kill|test_interactive_pty_manager_lifecycle|test_run_command_parses_timeout_seconds_argument|test_run_command_pty_cancelled_by_worker_token)|harness::tests::test_harness_delegate_task_manager_routes_and_returns_deliverable|orchestrator::notice_tests::test_handle_reply_to_arbitrator_ask_followup)$')
chk "$(printf '%s' "$unsanctioned" | grep -c .)" 0 "full lib suite: failures confined to the sanctioned environmental set"
echo "    $(grep -E '^test result' /tmp/t7_full.out)"

# --- scope invariants -----------------------------------------------------
grep -rn '"/bin/' $scope >/tmp/t7_bin.out;                                         chk $? 1 "rule 5: no \"/bin/\" paths in scope (t-019/t-024 intact)"
grep -rnP '[åäöÅÄÖ]' $scope >/dev/null 2>&1;                                       chk $? 1 "rule 3: no Swedish prose left in scope"
grep -rnE '^[[:space:]]*//+[[:space:]]*[─━=-]{3,}[[:space:]]*$' $scope >/dev/null; chk $? 1 "rule 2: no rule-only banner lines"
grep -rnE '(//).*\.rs:[0-9]+' $scope >/dev/null;                                   chk $? 1 "rule 2: no file:line pointers in comments"
chk "$(grep -c 'METHOD_TOOLS_CALL' src/mcp/http_tests.rs)" 6 "rule 5: METHOD_TOOLS_CALL stays a constant (t-021)"
chk "$(grep -c 'include_str!' src/mcp/http_tests.rs)" 3 "rule 6: include_str! self-guards intact"
untouched=0
for f in $(git ls-files $scope); do git diff --quiet HEAD -- "$f" && untouched=$((untouched+1)); done
chk "$untouched" 7 "scope: 7 of 16 files byte-identical to HEAD (comment-only edits in the other 9)"

# --- zero code-token change outside the sanctioned portability/fmt regions -
python3 - <<'PY'
import re, subprocess
from collections import Counter
scope = ["src/llm", "src/net", "src/mcp"]
files = subprocess.run(["git", "ls-files"] + scope, capture_output=True, text=True).stdout.split()
TOK = re.compile(r"""r?"[^\n"]*"|'[^'\n]*'|[A-Za-z_]\w*|\d+(?:\.\d+)?|[^\s\w]""")


def strip(t):
    """String-aware comment stripper (whole-line and trailing)."""
    out, i, n = [], 0, len(t)
    while i < n:
        m = re.match(r'r(#*)"', t[i:])
        if m:
            term = '"' + m.group(1)
            k = t.find(term, i + m.end())
            if k == -1:
                out.append(t[i:]); break
            out.append(t[i:k + len(term)]); i = k + len(term); continue
        if t[i] in '"\'':
            c, j = t[i], i + 1
            while j < n and t[j] != c and t[j] != "\n":
                j += 2 if t[j] == "\\" else 1
            out.append(t[i:j + 1]); i = j + 1; continue
        if t.startswith("//", i):
            k = t.find("\n", i); i = n if k == -1 else k; continue
        if t.startswith("/*", i):
            k = t.find("*/", i + 2); i = n if k == -1 else k + 2; continue
        out.append(t[i]); i += 1
    return "".join(out)


tok = lambda s: TOK.findall(strip(s))
head = lambda p: subprocess.run(["git", "show", f"HEAD:{p}"], capture_output=True,
                                text=True).stdout
# the ONLY files allowed to differ from HEAD, and the region that explains it
regions = {"src/llm/client_tests.rs": "t-024 (\"/bin/sh\" -> \"sh\")",
           "src/mcp/http_tests.rs": "t-021 (\"tools/call\" -> METHOD_TOOLS_CALL)",
           "src/mcp/client.rs": "t-019 (bare PATH program names + cfg(windows) branch)"}
bad = []
for p in files:
    a, b = Counter(tok(head(p))), Counter(tok(open(p, encoding="utf-8").read()))
    rem, add = list((a - b).elements()), list((b - a).elements())
    if not rem and not add:
        continue
    if p not in regions:
        bad.append(p)
        print(f"  FAIL unexplained token delta {p}: -{len(rem)} +{len(add)}")
    else:
        print(f"  token delta {p}: -{len(rem)} +{len(add)} -> {regions[p]}")
print(f"{'PASS' if not bad else 'FAIL'}  rule 1: code tokens unchanged outside t-019/t-021/t-024/fmt regions {bad or ''}")
if bad:
    raise SystemExit(1)

# rule 4: every string/byte literal containing non-ASCII (the UTF-8 fixtures:
# emoji, CJK, åäö, ...) must be byte-identical to HEAD across the whole scope.
bad4 = []
for p in files:
    def lits(t):
        return Counter(x for x in TOK.findall(t) if len(x) > 1 and any(ord(c) > 127 for c in x))
    ha, wb = lits(head(p)), lits(open(p, encoding="utf-8").read())
    if ha != wb:
        bad4.append(p)
print(f"{'PASS' if not bad4 else 'FAIL'}  rule 4: non-ASCII string fixtures byte-identical to HEAD {bad4 or ''}")
if bad4:
    raise SystemExit(1)
PY
python_rc=$?

# --- evidence dump (informational; does not affect the PASS/FAIL counters) ---
echo "--- evidence for the record ---"
echo "include_str! sites (rule 6):"; grep -n 'include_str!' src/mcp/http_tests.rs | cut -c1-100
echo "mcp/client.rs production/test split marker:"; grep -n '^mod tests {' src/mcp/client.rs
python3 - <<'PY'
needles = ['jsonrpc:"2.0"', '"initialize"', '"notifications/initialized"', '"tools/list"',
           '"tools/call"', 'MCP_PROTOCOL_VERSION', '2024-11-05', 'AtomicU64', '"inputSchema"',
           '"isError"', 'reqwest::Client::builder']
prod = [open("src/mcp/http.rs", encoding="utf-8").read(),
        open("src/mcp/client.rs", encoding="utf-8").read().split("#[cfg(test)]\nmod tests {")[0]]
print("production-half forbidden needles =",
      sum(p.count(n) for p in prod for n in needles),
      "| shared reqwest builder used in http.rs:", "build_http_client_with(" in prod[0])
PY
echo "UTF-8 emoji fixture, work tree vs HEAD (content must match):"
grep -o '"[^"]*🔍[^"]*"' src/llm/thinking_tests.rs | sort > /tmp/t7_fx_wt.txt
git show HEAD:src/llm/thinking_tests.rs | grep -o '"[^"]*🔍[^"]*"' | sort > /tmp/t7_fx_head.txt
cmp /tmp/t7_fx_head.txt /tmp/t7_fx_wt.txt && echo "  fixtures identical (diff empty), $(wc -l < /tmp/t7_fx_wt.txt) fixture lines"
echo "full-suite failures observed (sanctioned only):"
awk '/^failures:$/{f=1;next} /^test result/{f=0} f' /tmp/t7_full.out | grep -E '^ +[a-z_:]+$' | sort -u | sed 's/^/  /'

echo "t-007 one-shot summary: PASS=$pass FAIL=$fail python_rc=$python_rc"
[ "$fail" -eq 0 ] && [ "$python_rc" -eq 0 ]
