//! Unit tests for the search tools (`src/harness/search.rs`): gitignore-aware
//! grep, glob sorting, the truncation contract (recon bugs H8/H10) and brace
//! alternation (recon bug H10). Every test runs inside an isolated tempdir —
//! never the repo root.

use super::*;
use serde_json::Value;
use std::fs;
use std::path::{Path, PathBuf};

fn temp_dir() -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "marmel_search_{}_{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    fs::create_dir_all(&dir).unwrap();
    dir
}

/// Run the `glob` tool with `dir` as the *scoped* workspace root, so no test
/// ever walks the repository it is running from.
fn glob_at(dir: &Path, args: Value) -> Result<ToolResult, ToolError> {
    let dir = dir.to_path_buf();
    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .expect("current-thread runtime for a scoped workspace root");
    rt.block_on(crate::harness::with_workspace_root(dir, async move {
        glob(&args)
    }))
}

/// Build a tree with `files` files, each holding `lines` matching lines.
fn seed_match_tree(dir: &Path, files: usize, lines: usize) {
    for n in 0..files {
        let mut body = String::new();
        for m in 0..lines {
            body.push_str(&format!("needle {n}-{m}\n"));
        }
        fs::write(dir.join(format!("file{n:03}.txt")), body).unwrap();
    }
}

/// REQ-TOOL-005: grep_search honors `.gitignore` rules via the `ignore`
/// crate — ignored files' matches are not returned.
#[test]
fn test_harness_grep_gitignore() {
    let dir = temp_dir();
    fs::write(dir.join(".gitignore"), "ignored.log\n").unwrap();
    fs::write(dir.join("keep.txt"), "needle here\n").unwrap();
    fs::write(dir.join("ignored.log"), "needle in ignored\n").unwrap();

    let args = serde_json::json!({
        "pattern": "needle",
        "path": dir.to_string_lossy(),
        "max_results": 100,
    });
    let r = grep_search(&args).unwrap();
    let out = r.content;
    assert!(
        !out.contains("ignored.log"),
        "ignored file must not match, got:\n{out}"
    );
    assert!(
        out.contains("keep.txt"),
        "tracked file must match, got:\n{out}"
    );
    assert!(
        !out.contains(GREP_TRUNCATION_MARKER),
        "a complete answer must not be reported as truncated, got:\n{out}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// REQ-TOOL-006: glob returns relative paths sorted alphabetically.
#[test]
fn test_harness_glob_sorting() {
    let dir = temp_dir();
    fs::write(dir.join("zeta.rs"), "").unwrap();
    fs::write(dir.join("alpha.rs"), "").unwrap();
    fs::write(dir.join("beta.rs"), "").unwrap();

    let outcome = glob_in_root("*.rs", &dir, GLOB_HARD_CAP).unwrap();
    assert_eq!(
        outcome.matches,
        vec!["alpha.rs", "beta.rs", "zeta.rs"],
        "sorted"
    );
    assert_eq!(outcome.total_matches, 3, "exact total");
    assert!(!outcome.truncated(), "nothing was clipped");

    fs::remove_dir_all(&dir).unwrap();
}

// ── truncation contract (defect 1 / recon H8 + H10) ───────────────────────

/// (a) grep_search: when `max_results` is hit, the answer carries an explicit
/// `truncated: true` marker plus the exact total, so a caller can never mistake
/// a partial view for the complete set (recon H8).
#[test]
fn grep_truncation_reports_marker_flag_and_exact_total() {
    let dir = temp_dir();
    seed_match_tree(&dir, 4, 3); // 12 matches total

    let args = serde_json::json!({
        "pattern": "needle",
        "path": dir.to_string_lossy(),
        "max_results": 5,
    });
    let r = grep_search(&args).unwrap();
    let out = r.content;

    assert!(
        out.contains(GREP_TRUNCATION_MARKER),
        "marker present:\n{out}"
    );
    assert!(out.contains("truncated: true"), "flag present:\n{out}");
    assert!(out.contains("total_matches: 12"), "exact total:\n{out}");
    assert!(
        out.contains("returned_matches: 5"),
        "returned count:\n{out}"
    );
    assert!(out.contains("hidden_matches: 7"), "hidden count:\n{out}");
    assert!(out.contains("max_results: 5"), "effective cap:\n{out}");
    assert!(out.contains("ordering:"), "ordering documented:\n{out}");
    assert!(
        out.contains("Narrow the search"),
        "narrow-the-search hint present:\n{out}"
    );
    assert_eq!(
        out.lines().filter(|l| l.contains("needle ")).count(),
        5,
        "exactly max_results match lines are emitted"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// (b) grep_search: when everything fits, no truncation vocabulary appears and
/// the payload keeps the historical `path:line: text` join.
#[test]
fn grep_without_truncation_keeps_legacy_bytes() {
    let dir = temp_dir();
    seed_match_tree(&dir, 2, 2); // 4 matches

    let args = serde_json::json!({
        "pattern": "needle",
        "path": dir.to_string_lossy(),
        "max_results": 100,
    });
    let out = grep_search(&args).unwrap().content;

    assert!(
        !out.contains("truncated"),
        "no truncation vocabulary:\n{out}"
    );
    assert!(!out.contains(GREP_TRUNCATION_MARKER), "no marker:\n{out}");
    assert_eq!(out.lines().count(), 4, "one line per match:\n{out}");
    assert!(
        out.contains(":1: needle 0-0"),
        "legacy `path:line: text` shape kept:\n{out}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// (c) The cap decision is deterministic: two independent runs of the same
/// search/glob over the same tree produce byte-identical output.
#[test]
fn grep_and_glob_truncation_is_deterministic_across_runs() {
    let dir = temp_dir();
    seed_match_tree(&dir, 6, 4); // 24 matches in 6 .txt files
    for n in 0..6 {
        fs::write(dir.join(format!("extra{n:03}.md")), "needle md\n").unwrap();
    }
    fs::create_dir_all(dir.join("adir")).unwrap();
    fs::write(dir.join("adir/first.txt"), "needle adir\n").unwrap();
    fs::create_dir_all(dir.join("zdir/deeper")).unwrap();
    fs::write(dir.join("zdir/deeper/last.txt"), "needle zdir\n").unwrap();

    let args = serde_json::json!({
        "pattern": "needle",
        "path": dir.to_string_lossy(),
        "max_results": 7,
    });
    let first = grep_search(&args).unwrap().content;
    let second = grep_search(&args).unwrap().content;
    assert_eq!(first, second, "grep output must be stable across runs");
    assert_eq!(
        first.lines().filter(|l| l.contains("needle ")).count(),
        7,
        "the same 7 matches are kept every run"
    );
    assert!(
        first
            .lines()
            .next()
            .is_some_and(|l| l.contains("adir/first.txt")),
        "documented order: siblings sorted by path, so adir/ comes first, got:\n{first}"
    );
    assert!(
        first.contains("total_matches: 32"),
        "nested matches are counted too:\n{first}"
    );

    let a = glob_in_root("**/*.{txt,md}", &dir, 5).unwrap();
    let b = glob_in_root("**/*.{txt,md}", &dir, 5).unwrap();
    assert_eq!(a.matches, b.matches, "glob list must be stable across runs");
    assert_eq!(a.matches.len(), 5, "cap respected");
    assert_eq!(a.total_matches, 14, "exact total across both alternatives");
    assert_eq!(
        a.matches,
        vec![
            "adir/first.txt",
            "extra000.md",
            "extra001.md",
            "extra002.md",
            "extra003.md",
        ],
        "the lexicographically first paths survive the cap"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// A `max_results` above the hard cap is reported, not silently swallowed.
#[test]
fn grep_reports_when_max_results_was_clamped_to_the_hard_cap() {
    let dir = temp_dir();
    seed_match_tree(&dir, GREP_HARD_CAP / 2 + 1, 2); // more than GREP_HARD_CAP matches

    let args = serde_json::json!({
        "pattern": "needle",
        "path": dir.to_string_lossy(),
        "max_results": 5_000usize,
    });
    let out = grep_search(&args).unwrap().content;

    assert!(out.contains(GREP_TRUNCATION_MARKER), "clipped:\n{out}");
    assert!(
        out.contains(&format!("max_results: {GREP_HARD_CAP}")),
        "effective cap stated:\n{out}"
    );
    assert!(
        out.contains("max_results_requested: 5000 clamped_to: 500"),
        "the clamp is visible:\n{out}"
    );
    assert_eq!(
        out.lines().filter(|l| l.contains("needle ")).count(),
        GREP_HARD_CAP
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// (a)/(b) glob side of the same contract, driven through the real tool entry
/// point: marker + counts when clipped, no truncation vocabulary when the whole
/// match set fits.
#[test]
fn glob_truncation_reports_marker_flag_and_exact_total() {
    let dir = temp_dir();
    for n in 0..7 {
        fs::write(dir.join(format!("f{n}.rs")), "x\n").unwrap();
    }

    let clipped = glob_at(
        &dir,
        serde_json::json!({"pattern": "*.rs", "max_results": 4}),
    )
    .unwrap();
    let out = clipped.content;
    assert!(
        out.contains(GLOB_TRUNCATION_MARKER),
        "marker present:\n{out}"
    );
    assert!(out.contains("truncated: true"), "flag present:\n{out}");
    assert!(out.contains("total_matches: 7"), "exact total:\n{out}");
    assert!(out.contains("returned_paths: 4"), "returned count:\n{out}");
    assert!(out.contains("hidden_matches: 3"), "hidden count:\n{out}");
    assert!(out.contains("max_results: 4"), "effective cap:\n{out}");
    assert!(
        out.contains("pattern_alternatives: 1"),
        "alternatives:\n{out}"
    );
    assert!(
        out.contains("the list above is NOT complete"),
        "the partial view is called out:\n{out}"
    );
    assert_eq!(
        out.lines().filter(|l| l.ends_with(".rs")).count(),
        4,
        "exactly max_results paths are emitted"
    );

    let fits = glob_at(&dir, serde_json::json!({"pattern": "*.rs"})).unwrap();
    assert!(
        !fits.content.contains("truncated"),
        "no footer:\n{}",
        fits.content
    );
    assert_eq!(
        fits.content.lines().filter(|l| l.ends_with(".rs")).count(),
        7,
        "default cap keeps everything"
    );
    assert!(
        !fits.is_error,
        "a complete answer is not an error: {}",
        fits.content
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// `max_results: 0` yields a footer-only answer: the caller is told matches
/// exist, without a bogus "no matches" claim.
#[test]
fn grep_with_zero_results_cap_still_reports_the_matches_that_exist() {
    let dir = temp_dir();
    seed_match_tree(&dir, 2, 2);

    let out = grep_search(&serde_json::json!({
        "pattern": "needle",
        "path": dir.to_string_lossy(),
        "max_results": 0,
    }))
    .unwrap()
    .content;

    assert!(out.starts_with(GREP_TRUNCATION_MARKER), "got:\n{out}");
    assert!(out.contains("truncated: true"));
    assert!(out.contains("total_matches: 4"));
    assert!(out.contains("returned_matches: 0"));

    fs::remove_dir_all(&dir).unwrap();
}

// ── brace alternation (defect 2 / recon H10) ──────────────────────────────

/// (d) `**/*.{rs,md}` matches both extensions and yields each path exactly once.
#[test]
fn glob_brace_pattern_matches_every_extension_without_duplicates() {
    let dir = temp_dir();
    fs::create_dir_all(dir.join("src/deep")).unwrap();
    for name in [
        "top.rs",
        "top.md",
        "top.toml",
        "src/a.rs",
        "src/a.md",
        "src/deep/b.rs",
        "src/deep/b.md",
        "src/deep/keep.txt",
    ] {
        fs::write(dir.join(name), "x\n").unwrap();
    }

    // Through the real tool, exactly the shape from the bug report.
    let via_tool = glob_at(&dir, serde_json::json!({"pattern": "src/**/*.{rs,md}"})).unwrap();
    assert_eq!(
        via_tool.content.lines().collect::<Vec<_>>(),
        vec!["src/a.md", "src/a.rs", "src/deep/b.md", "src/deep/b.rs"],
        "the brace pattern must match both extensions"
    );

    let outcome = glob_in_root("**/*.{rs,md}", &dir, GLOB_HARD_CAP).unwrap();
    assert_eq!(outcome.alternatives, 2, "two alternative patterns");
    assert_eq!(
        outcome.matches,
        vec![
            "src/a.md",
            "src/a.rs",
            "src/deep/b.md",
            "src/deep/b.rs",
            "top.md",
            "top.rs"
        ],
        "both extensions, lexicographically sorted, no duplicates"
    );
    assert_eq!(
        outcome.total_matches, 6,
        "a path matched by several alternatives counts once"
    );
    assert_eq!(
        outcome.matches.len(),
        outcome
            .matches
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len(),
        "de-duplicated"
    );

    let toml = glob_in_root("*.toml", &dir, GLOB_HARD_CAP).unwrap();
    assert_eq!(
        toml.matches,
        vec!["top.toml"],
        "control: the third extension exists in the tree"
    );
    // The pre-fix behaviour this test defends against: braces escaped literally.
    assert!(
        !glob_in_root("**/*.rs", &dir, GLOB_HARD_CAP)
            .unwrap()
            .matches
            .contains(&"top.md".to_string()),
        "a brace-free pattern must not be treated as a brace pattern"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// (e) Nested alternation behaves exactly as documented: `{a,{b,c}}` is the
/// union `a | b | c`, left-to-right and de-duplicated.
#[test]
fn glob_nested_braces_expand_left_to_right_and_match_the_union() {
    assert_eq!(
        expand_braces("{a,{b,c}}").unwrap(),
        vec!["a", "b", "c"],
        "left-to-right, de-duplicated"
    );
    assert_eq!(
        expand_braces("src/{main,tests}/*.{rs,md}").unwrap(),
        vec![
            "src/main/*.rs",
            "src/main/*.md",
            "src/tests/*.rs",
            "src/tests/*.md"
        ],
        "expansion order is part of the contract"
    );
    assert_eq!(
        expand_braces("{a,a,{b,a}}").unwrap(),
        vec!["a", "b"],
        "identical alternatives collapse"
    );
    assert_eq!(
        expand_braces("*.{rs}").unwrap(),
        vec!["*.rs"],
        "a one-option group is still alternation"
    );
    assert_eq!(
        expand_braces("a/b,c.txt").unwrap(),
        vec!["a/b,c.txt"],
        "a comma outside a group is literal"
    );
    assert_eq!(
        expand_braces("\\{a,b\\}").unwrap(),
        vec!["\\{a,b\\}"],
        "escaped braces stay literal text"
    );
    assert_eq!(
        expand_braces("plain/**/*.rs").unwrap(),
        vec!["plain/**/*.rs"],
        "brace-free patterns pass through untouched"
    );

    let dir = temp_dir();
    for name in ["x1.txt", "y1.txt", "z1.txt", "q1.txt"] {
        fs::write(dir.join(name), "x\n").unwrap();
    }
    let outcome = glob_in_root("{x,{y,z}}*.txt", &dir, GLOB_HARD_CAP).unwrap();
    assert_eq!(outcome.matches, vec!["x1.txt", "y1.txt", "z1.txt"]);
    assert_eq!(outcome.alternatives, 3);
    assert_eq!(outcome.total_matches, 3);

    fs::remove_dir_all(&dir).unwrap();
}

/// (f) Malformed braces produce an explicit error — never an empty list.
#[test]
fn glob_malformed_braces_is_an_explicit_error_not_an_empty_result() {
    let dir = temp_dir();
    fs::write(dir.join("a.rs"), "x\n").unwrap();
    fs::write(dir.join("a.md"), "x\n").unwrap();

    for bad in [
        "src/**/*.{rs,md", // unbalanced `{` — the shape from the bug report
        "{rs,md",          // unbalanced `{`
        "*.{rs",           // unbalanced `{`
        "*.{},a.rs",       // empty group
        "{}",              // empty group
        "{a,{b,{c,d}",     // unbalanced after nesting
    ] {
        let err = glob_in_root(bad, &dir, GLOB_HARD_CAP)
            .expect_err("{bad} must be rejected, not answered with no matches");
        let text = err.to_string();
        assert!(
            text.contains("brace") || text.contains("malformed"),
            "{bad}: error must name the brace problem, got {text}"
        );
        assert!(
            !text.contains("no matches"),
            "{bad}: must not be dressed up as an empty result: {text}"
        );
    }

    // The error is a *validation* error carrying the tool name.
    let err = glob_in_root("src/**/*.{rs,md", &dir, GLOB_HARD_CAP)
        .expect_err("unbalanced brace must error");
    match err {
        ToolError::BadArguments { tool, detail } => {
            assert_eq!(tool, TOOL_GLOB, "attributed to the glob tool");
            assert!(
                detail.contains("unbalanced"),
                "detail names the defect: {detail}"
            );
        }
        other => panic!("expected BadArguments, got {other:?}"),
    }

    // Through the tool entry point the same pattern is an error result, not a
    // confident "no matches".
    let err = glob_at(&dir, serde_json::json!({"pattern": "src/**/*.{rs,md"}))
        .expect_err("the tool must surface the validation error");
    assert!(err.to_string().contains("brace"), "got: {err}");

    // A balanced one-option group is still valid.
    assert!(glob_in_root("*.{rs}", &dir, GLOB_HARD_CAP).is_ok());

    // Over-nesting and combinatorial blow-up are rejected rather than truncated.
    let deep = format!(
        "{}a{}",
        "{".repeat(MAX_GLOB_BRACE_DEPTH + 1),
        "}".repeat(MAX_GLOB_BRACE_DEPTH + 1)
    );
    let deep_err = expand_braces(&deep).expect_err("over-nested braces must error");
    assert!(deep_err.contains("nested"), "got: {deep_err}");

    // How many 2-option groups produce exactly MAX_GLOB_ALTERNATIVES forms?
    let mut groups = 0usize;
    let mut forms = 1usize;
    while forms < MAX_GLOB_ALTERNATIVES {
        forms *= 2;
        groups += 1;
    }
    if forms == MAX_GLOB_ALTERNATIVES {
        assert_eq!(
            expand_braces(&"{a,b}".repeat(groups)).map(|v| v.len()),
            Ok(MAX_GLOB_ALTERNATIVES),
            "at the ceiling the expansion still succeeds"
        );
        assert!(
            expand_braces(&"{a,b}".repeat(groups + 1)).is_err(),
            "one group past the ceiling must error"
        );
    }
    assert!(
        expand_braces(&"{a,b}".repeat(MAX_GLOB_ALTERNATIVES + 1)).is_err(),
        "absurd expansion must error"
    );

    // A stray `}` is literal text (shell/globset semantics), not an error.
    assert_eq!(expand_braces("a}.rs").unwrap(), vec!["a}.rs"]);

    fs::remove_dir_all(&dir).unwrap();
}

/// (g) A valid pattern that matches nothing answers honestly: no error, no
/// truncation vocabulary, the historical `no matches` text.
#[test]
fn glob_matching_nothing_is_an_honest_empty_result() {
    let dir = temp_dir();
    fs::write(dir.join("a.rs"), "x\n").unwrap();
    fs::create_dir_all(dir.join("empty.d")).unwrap();

    for pattern in ["*.md", "*.{md,toml}", "nope/**/*.rs", "zeta.rs"] {
        let outcome = glob_in_root(pattern, &dir, GLOB_HARD_CAP)
            .unwrap_or_else(|e| panic!("{pattern} is well formed, got error: {e}"));
        assert!(outcome.matches.is_empty(), "{pattern}: empty list");
        assert_eq!(outcome.total_matches, 0, "{pattern}: exact zero");
        assert!(!outcome.truncated(), "{pattern}: nothing was hidden");
    }

    // End to end through the tool entry point: the literal `no matches` string,
    // byte-identical to the pre-fix answer.
    let r = glob_at(
        &dir,
        serde_json::json!({"pattern": "no-such-extension.zzz"}),
    )
    .unwrap();
    assert!(!r.is_error, "an empty answer is not an error");
    assert_eq!(r.content, "no matches");

    fs::remove_dir_all(&dir).unwrap();
}

/// `grep_search` keeps `pattern` as its argument key and never reintroduces the
/// `query` spelling in what it emits.
#[test]
fn grep_search_reads_the_pattern_key_and_never_echoes_query() {
    let dir = temp_dir();
    fs::write(dir.join("notes.txt"), "needle line\n").unwrap();

    let out = grep_search(&serde_json::json!({"pattern": "needle", "path": dir.to_string_lossy()}))
        .unwrap()
        .content;
    assert!(out.contains("notes.txt:1: needle line"), "got:\n{out}");
    assert!(
        !out.contains("query"),
        "the legacy preview key must not leak into results:\n{out}"
    );

    fs::remove_dir_all(&dir).unwrap();
}

/// The `**` and `?` translation is untouched by the brace work, and `\X` now
/// matches a literal character so real braces in file names stay addressable.
#[test]
fn glob_translates_star_question_and_escapes_as_documented() {
    let dir = temp_dir();
    fs::create_dir_all(dir.join("nested/dir")).unwrap();
    fs::write(dir.join("nested/dir/odd.rs"), "x\n").unwrap();
    fs::write(dir.join("nested/a1.rs"), "x\n").unwrap();
    fs::write(dir.join("nested/a12.rs"), "x\n").unwrap();
    fs::write(dir.join("{literal}.rs"), "x\n").unwrap();

    assert_eq!(
        glob_in_root("**/odd.rs", &dir, GLOB_HARD_CAP)
            .unwrap()
            .matches,
        vec!["nested/dir/odd.rs"]
    );
    assert_eq!(
        glob_in_root("nested/a?.rs", &dir, GLOB_HARD_CAP)
            .unwrap()
            .matches,
        vec!["nested/a1.rs"]
    );
    assert_eq!(
        glob_in_root("\\{literal\\}.rs", &dir, GLOB_HARD_CAP)
            .unwrap()
            .matches,
        vec!["{literal}.rs"],
        "escaped braces match a literal brace in the name"
    );

    fs::remove_dir_all(&dir).unwrap();
}
