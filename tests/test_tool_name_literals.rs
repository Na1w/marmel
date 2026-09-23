//! Guard test (refactor t-c1v9): tool-name string literals must reference the
//! constants in `src/tool_names.rs` instead of raw literals.
//!
//! Implementation: scan every `src/**/*.rs` file (except `tool_names.rs`
//! itself) for raw `"tool_name"` literals from a maintained list of known tool
//! names, and assert the per-file counts match a maintained baseline of
//! *intentionally* remaining literals (wire-text fixtures, doc comments, TOML
//! test strings, and log-content assertions that must stay byte-identical).
//!
//! If you add a new raw tool-name literal to `src/`, this test fails — either
//! use the constant from `marmennill::tool_names`, or (only if the literal is
//! genuinely intentional) add it to the baseline table below with a comment.

use std::path::Path;

/// Known tool names (bare + caesar `terminal__` variants) that must not appear
/// as raw string literals outside `src/tool_names.rs`.
const KNOWN_TOOL_NAMES: &[&str] = &[
    "read_file",
    "write_file",
    "replace",
    "run_command",
    "grep_search",
    "glob",
    "create_plan",
    "archive_current_plan",
    "rebirth",
    "pty_spawn",
    "pty_write",
    "pty_read",
    "pty_close",
    "pty_list",
    "leave_verdict",
    "sleep",
    "delegate_task",
    "list_directory",
    "terminal__read_file",
    "terminal__write_file",
    "terminal__replace",
    "terminal__run_command",
    "terminal__grep_search",
    "terminal__glob",
    "terminal__list_directory",
    "terminal__sleep",
    "terminal__leave_verdict",
];

/// Intentionally remaining raw literals: (file, tool name, exact count).
/// Every entry must be a wire-text fixture, doc comment, TOML test string, or
/// an assertion on rendered log/output text that must stay byte-identical.
const BASELINE: &[(&str, &str, usize)] = &[
    // TOML config fixture string in a test (config file text, not code).
    ("src/config.rs", "delegate_task", 2),
    // Assertion that the rendered debug log *contains* the tool name text.
    ("src/debug_log.rs", "delegate_task", 1),
    // XML rescue wire-text fixtures (raw model output being parsed).
    ("src/harness/monitor/monitor_tests.rs", "glob", 1),
    ("src/harness/monitor/monitor_tests.rs", "read_file", 2),
    ("src/harness/monitor/monitor_tests.rs", "write_file", 1),
    // Doc-comment examples of wire format.
    ("src/harness/monitor/repetition.rs", "read_file", 2),
    // Doc-comment + inline comment examples of wire format.
    ("src/harness/monitor/xml.rs", "read_file", 4),
    ("src/harness/monitor/xml.rs", "write_file", 1),
    // XML rescue wire-text fixtures (raw model output being parsed).
    ("src/manager/loop_tests.rs", "glob", 2),
    ("src/manager/loop_tests.rs", "read_file", 1),
    // Inputs to `normalize_steer_decision` (accepts raw LLM decision strings).
    ("src/orchestrator/steer_tests.rs", "delegate_task", 1),
    ("src/orchestrator/steer_tests.rs", "sleep", 1),
    // Wire-format JSON fixture (serialized tool call shape).
    ("src/types/wire.rs", "read_file", 1),
];

fn count_literal(content: &str, tool_name: &str) -> usize {
    let needle = format!("\"{tool_name}\"");
    content.matches(&needle).count()
}

fn walk_src(dir: &Path, out: &mut Vec<(String, String)>) {
    for entry in dir.read_dir().expect("read src/") {
        let entry = entry.unwrap();
        let path = entry.path();
        if path.is_dir() {
            walk_src(&path, out);
        } else if path.extension().is_some_and(|e| e == "rs")
            && path.file_name().is_some_and(|f| f != "tool_names.rs")
        {
            // Walk starts at `src/`, so the path is already workspace-relative.
            let rel = path.to_string_lossy().replace('\\', "/");
            out.push((rel, std::fs::read_to_string(&path).unwrap()));
        }
    }
}

#[test]
fn test_no_unexpected_raw_tool_name_literals_in_src() {
    let mut files = Vec::new();
    walk_src(Path::new("src"), &mut files);
    assert!(
        !files.is_empty(),
        "guard test must find source files under src/"
    );

    let mut violations = Vec::new();
    for (file, content) in &files {
        for name in KNOWN_TOOL_NAMES {
            let actual = count_literal(content, name);
            let expected = BASELINE
                .iter()
                .find(|(f, n, _)| f == file && n == name)
                .map(|(_, _, c)| *c)
                .unwrap_or(0);
            if actual != expected {
                violations.push(format!(
                    "{file}: \"{name}\" literal count = {actual}, expected {expected} (baseline)"
                ));
            }
        }
    }

    assert!(
        violations.is_empty(),
        "raw tool-name literals drifted from baseline — use constants from \
         `marmennill::tool_names` (or extend the BASELINE table if intentional):\n{}",
        violations.join("\n")
    );
}
