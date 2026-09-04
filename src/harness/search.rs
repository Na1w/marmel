//! Search tools: grep_search (gitignore-aware) and glob (sorted, capped).
//!
//! REQ-TOOL-005: `grep_search` uses the `ignore` crate to traverse files,
//! honoring `.gitignore` rules. Results are capped at 500 (default 100).
//! REQ-TOOL-006: `glob` returns sorted relative paths capped at 500 entries.

use crate::harness::fs::{str_arg, usize_arg};
use crate::harness::{ToolError, ToolResult};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::Value;
use std::path::Path;

/// Hard cap on the number of grep results (REQ-TOOL-005).
pub const GREP_HARD_CAP: usize = 500;
/// Hard cap on the number of glob results (REQ-TOOL-006).
pub const GLOB_HARD_CAP: usize = 500;

/// `grep_search(pattern, path, max_results)` — regex search over files.
///
/// Walks `path` with the `ignore` crate (which honors `.gitignore`), reads each
/// file, and collects lines matching the regex. Returns at most `max_results`
/// matches, hard-capped at 500.
pub fn grep_search(args: &Value) -> Result<ToolResult, ToolError> {
    let pattern = str_arg(args, "pattern", "grep_search")?;
    let raw_root = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let safe_root = crate::harness::fs::resolve_safe_path(raw_root, "grep_search")?;
    let max_results = usize_arg(args, "max_results", 100, "grep_search")?.min(GREP_HARD_CAP);

    let re = Regex::new(pattern).map_err(|e| ToolError::BadArguments {
        tool: "grep_search".into(),
        detail: format!("invalid regex: {e}"),
    })?;

    let mut results = Vec::new();
    for entry in WalkBuilder::new(&safe_root)
        .require_git(false)
        .build()
        .flatten()
    {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            for (idx, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    results.push(format!("{}:{}: {}", path.display(), idx + 1, line));
                    if results.len() >= max_results {
                        return Ok(ToolResult::ok(results.join("\n")));
                    }
                }
            }
        }
    }

    if results.is_empty() {
        Ok(ToolResult::ok("no matches"))
    } else {
        Ok(ToolResult::ok(results.join("\n")))
    }
}

/// `glob(pattern)` — filesystem glob expansion, sorted, capped at 500.
///
/// Matches are relative paths. Root is the workspace root directory.
pub fn glob(args: &Value) -> Result<ToolResult, ToolError> {
    let pattern = str_arg(args, "pattern", "glob")?;
    let root = crate::harness::get_workspace_root();
    let matches = glob_in_root(pattern, &root);
    if matches.is_empty() {
        Ok(ToolResult::ok("no matches"))
    } else {
        Ok(ToolResult::ok(matches.join("\n")))
    }
}

/// Glob expansion rooted at `root`, returning paths relative to `root`,
/// sorted alphabetically and capped at 500.
pub(crate) fn glob_in_root(pattern: &str, root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let regex = glob_to_regex(pattern);
    for entry in WalkBuilder::new(root).require_git(false).build().flatten() {
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
        if regex.is_match(&rel) {
            out.push(rel);
        }
    }
    out.sort();
    out.truncate(GLOB_HARD_CAP);
    out
}

/// Translate a glob pattern into an anchored regex.
fn glob_to_regex(pattern: &str) -> Regex {
    let mut re = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
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
    Regex::new(&re).unwrap_or_else(|_| Regex::new("^$").unwrap())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> std::path::PathBuf {
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

        fs::remove_dir_all(&dir).unwrap();
    }

    /// REQ-TOOL-006: glob returns relative paths sorted alphabetically.
    #[test]
    fn test_harness_glob_sorting() {
        let dir = temp_dir();
        fs::write(dir.join("zeta.rs"), "").unwrap();
        fs::write(dir.join("alpha.rs"), "").unwrap();
        fs::write(dir.join("beta.rs"), "").unwrap();

        let files = glob_in_root("*.rs", &dir);
        assert_eq!(files, vec!["alpha.rs", "beta.rs", "zeta.rs"], "sorted");

        fs::remove_dir_all(&dir).unwrap();
    }
}
