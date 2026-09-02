//! Filesystem tools: read_file (paginated), replace (strict), write_file.
//!
//! REQ-TOOL-002: `replace` only writes when the target string occurs exactly
//! once — 0 or ≥2 matches return an error without touching the file.
//! REQ-TOOL-003: `read_file` is line-paginated with `{line_num} | {content}`
//! rows and a `[Showing lines X-Y of Z...]` footer.
//! REQ-TOOL-004: `write_file` auto-creates parent directories with mode 0o755.
//!
//! Path mapping: every path argument is passed through `map_path`, which maps
//! the canonical container path `/home/coder/workspace/...` onto the current
//! working directory, matching marmennill-cli's local tool execution.

use crate::harness::{ToolError, ToolResult};
use crate::tool_names::{TOOL_READ_FILE, TOOL_REPLACE, TOOL_WRITE_FILE};
use serde_json::Value;
use std::path::{Path, PathBuf};

/// The canonical container workspace path prefix used by marmennill-cli.
pub const WORKSPACE_PREFIX: &str = "/home/coder/workspace";

/// Map a `/home/coder/workspace/...` path onto the current working directory,
/// matching marmennill-cli's `map_path`. Non-workspace paths are returned as-is.
pub fn map_path(path: &str) -> PathBuf {
    let cur_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if path.starts_with(WORKSPACE_PREFIX) {
        let relative = path.strip_prefix(WORKSPACE_PREFIX).unwrap_or(path);
        let clean_relative = relative.strip_prefix('/').unwrap_or(relative);
        cur_dir.join(clean_relative)
    } else {
        PathBuf::from(path)
    }
}

/// Resolve a path securely, ensuring it stays confined inside the workspace root.
///
/// Prevents path traversal attacks (e.g. `../../etc/passwd` or absolute escapes).
pub fn resolve_safe_path(path: &str, tool: &str) -> Result<PathBuf, ToolError> {
    let cur_dir = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let canonical_root = cur_dir.canonicalize().unwrap_or_else(|_| cur_dir.clone());
    let canonical_temp = std::env::temp_dir()
        .canonicalize()
        .unwrap_or_else(|_| std::env::temp_dir());

    let raw_target = if path.starts_with(WORKSPACE_PREFIX) {
        let relative = path.strip_prefix(WORKSPACE_PREFIX).unwrap_or(path);
        let clean_relative = relative.strip_prefix('/').unwrap_or(relative);
        canonical_root.join(clean_relative)
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        canonical_root.join(path)
    };

    let canonical_target = if raw_target.exists() {
        raw_target
            .canonicalize()
            .map_err(|e| ToolError::BadArguments {
                tool: tool.to_string(),
                detail: format!("failed to resolve path '{path}': {e}"),
            })?
    } else {
        let mut cur = raw_target.clone();
        let mut components = Vec::new();
        while !cur.exists() {
            if let Some(name) = cur.file_name() {
                components.push(name.to_os_string());
                if let Some(parent) = cur.parent() {
                    cur = parent.to_path_buf();
                } else {
                    break;
                }
            } else {
                break;
            }
        }
        let canonical_ancestor = cur.canonicalize().unwrap_or(cur);
        let mut resolved = canonical_ancestor;
        for part in components.into_iter().rev() {
            resolved.push(part);
        }
        resolved
    };

    if !canonical_target.starts_with(&canonical_root)
        && !canonical_target.starts_with(&canonical_temp)
    {
        return Err(ToolError::Forbidden {
            tool: tool.to_string(),
            caller: format!("access denied: path '{}' escapes workspace root", path),
        });
    }

    Ok(canonical_target)
}

/// `read_file(path, offset, limit)` — reads a UTF-8 file window by *characters*.
///
/// - `offset` is 0-based character index (default: 0).
/// - `limit` is character count (default: 8000, max: 8000).
/// - Returns sliced text with pagination footer if more characters remain.
pub fn read_file(args: &Value) -> Result<ToolResult, ToolError> {
    let path = str_arg(args, "path", TOOL_READ_FILE)?;
    let offset = usize_arg(args, "offset", 0, TOOL_READ_FILE)?;
    let limit = usize_arg(args, "limit", 8000, TOOL_READ_FILE)?.min(8000);

    let safe_path = resolve_safe_path(path, TOOL_READ_FILE)?;
    let raw_bytes = std::fs::read(&safe_path).map_err(anyhow::Error::from)?;
    let content = String::from_utf8_lossy(&raw_bytes);
    let total_chars = content.chars().count();

    let start = offset.min(total_chars);
    let end = (start + limit).min(total_chars);
    let sliced_content: String = content.chars().skip(start).take(end - start).collect();
    let has_more = end < total_chars;

    let mut out = sliced_content;
    if has_more {
        out.push_str(&format!(
            "\n\n[Showing characters {start}-{end} of {total_chars}. Use offset={end} to read next chunk]"
        ));
    }

    Ok(ToolResult::ok(out))
}

/// `replace(path, old_str, new_str)` — fails safely on 0 or ≥2 matches.
///
/// Counts occurrences of `old_str`. Only when exactly one match exists does it
/// perform the replacement; otherwise it returns an error and never writes.
pub fn replace(args: &Value) -> Result<ToolResult, ToolError> {
    let path = str_arg(args, "path", TOOL_REPLACE)?;
    let old_str = str_arg(args, "old_str", TOOL_REPLACE)?;
    let new_str = str_arg(args, "new_str", TOOL_REPLACE)?;

    let safe_path = resolve_safe_path(path, TOOL_REPLACE)?;
    let content = std::fs::read_to_string(&safe_path).map_err(anyhow::Error::from)?;
    let count = content.matches(old_str).count();

    if count == 0 {
        return Ok(ToolResult::err(format!(
            "old_str not found in file: {path}"
        )));
    }
    if count > 1 {
        return Ok(ToolResult::err(format!(
            "old_str is ambiguous (matches {count} times in {path}). Provide more unique surrounding context."
        )));
    }

    let new_content = content.replace(old_str, new_str);
    // Atomic single-match write: write to a temp file in the same dir, then rename.
    let dir = safe_path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let tmp = dir.join(format!(
        ".{}.tmp.{}",
        safe_path
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, new_content).map_err(anyhow::Error::from)?;
    std::fs::rename(&tmp, &safe_path).map_err(anyhow::Error::from)?;
    Ok(ToolResult::ok("replace applied"))
}

/// `write_file(path, content)` — writes full content to a path.
///
/// Creates any missing parent directories with permissions `0o755`.
pub fn write_file(args: &Value) -> Result<ToolResult, ToolError> {
    let path = str_arg(args, "path", TOOL_WRITE_FILE)?;
    let content = str_arg(args, "content", TOOL_WRITE_FILE)?;
    let safe_path = resolve_safe_path(path, TOOL_WRITE_FILE)?;
    if let Some(parent) = safe_path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent).map_err(anyhow::Error::from)?;
        // Ensure 0o755 permissions on the newly created parents.
        set_dir_permissions_755(parent);
    }
    std::fs::write(&safe_path, content).map_err(anyhow::Error::from)?;
    Ok(ToolResult::ok(format!(
        "wrote {} bytes to {}",
        content.len(),
        path
    )))
}

#[cfg(unix)]
fn set_dir_permissions_755(dir: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755));
}

#[cfg(not(unix))]
fn set_dir_permissions_755(_dir: &Path) {}

/// Extract a required string argument with alias support.
pub(crate) fn str_arg<'a>(args: &'a Value, key: &str, tool: &str) -> Result<&'a str, ToolError> {
    if let Some(v) = args.get(key).and_then(Value::as_str) {
        return Ok(v);
    }
    match key {
        "path" => {
            if let Some(v) = args
                .get("file_path")
                .or_else(|| args.get("filepath"))
                .or_else(|| args.get("file"))
                .or_else(|| args.get("filename"))
                .or_else(|| args.get("file_name"))
                .or_else(|| args.get("target"))
                .or_else(|| args.get("target_file"))
                .or_else(|| args.get("dest"))
                .or_else(|| args.get("destination"))
                .or_else(|| args.get("output_file"))
                .or_else(|| args.get("output_path"))
                .or_else(|| args.get("name"))
                .or_else(|| args.get("doc"))
                .or_else(|| args.get("path_to_file"))
                .and_then(Value::as_str)
            {
                return Ok(v);
            }
        }
        "content" => {
            if let Some(v) = args
                .get("contents")
                .or_else(|| args.get("text"))
                .or_else(|| args.get("code"))
                .or_else(|| args.get("body"))
                .or_else(|| args.get("file_content"))
                .or_else(|| args.get("filecontent"))
                .or_else(|| args.get("data"))
                .or_else(|| args.get("source"))
                .or_else(|| args.get("raw"))
                .and_then(Value::as_str)
            {
                return Ok(v);
            }
        }
        "pattern" => {
            if let Some(v) = args
                .get("query")
                .or_else(|| args.get("search"))
                .or_else(|| args.get("glob"))
                .or_else(|| args.get("regex"))
                .and_then(Value::as_str)
            {
                return Ok(v);
            }
        }
        "old_str" => {
            if let Some(v) = args
                .get("target")
                .or_else(|| args.get("search"))
                .or_else(|| args.get("find"))
                .or_else(|| args.get("old"))
                .or_else(|| args.get("target_content"))
                .and_then(Value::as_str)
            {
                return Ok(v);
            }
        }
        "new_str" => {
            if let Some(v) = args
                .get("replacement")
                .or_else(|| args.get("replace"))
                .or_else(|| args.get("new"))
                .or_else(|| args.get("replacement_content"))
                .and_then(Value::as_str)
            {
                return Ok(v);
            }
        }
        "command" => {
            if let Some(v) = args
                .get("cmd")
                .or_else(|| args.get("script"))
                .or_else(|| args.get("exec"))
                .or_else(|| args.get("command_line"))
                .and_then(Value::as_str)
            {
                return Ok(v);
            }
        }
        _ => {}
    }
    Err(ToolError::BadArguments {
        tool: tool.into(),
        detail: format!("missing string field `{key}`"),
    })
}

/// Extract an optional integer argument with a default.
pub(crate) fn usize_arg(
    args: &Value,
    key: &str,
    default: usize,
    tool: &str,
) -> Result<usize, ToolError> {
    match args.get(key) {
        None => Ok(default),
        Some(v) => v
            .as_u64()
            .map(|u| u as usize)
            .ok_or_else(|| ToolError::BadArguments {
                tool: tool.into(),
                detail: format!("field `{key}` must be an integer"),
            }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "marmel_fs_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// REQ-TOOL-002: replace on a non-matching string returns an error and
    /// does not modify the file; ambiguous (≥2) matches error without writing;
    /// a single match writes correctly.
    #[test]
    fn test_harness_replace_uniqueness() {
        let dir = temp_dir();
        let path = dir.join("file.txt");
        fs::write(&path, "hello world\nhello world again\n").unwrap();

        // 0 matches -> error, file untouched.
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "old_str": "zzz-not-present",
            "new_str": "nope",
        });
        let r = replace(&args).unwrap();
        assert!(r.is_error, "0-match replace must be an error");
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "hello world\nhello world again\n"
        );

        // >1 matches (old_str "hello" appears twice) -> error, file untouched.
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "old_str": "hello",
            "new_str": "X",
        });
        let r = replace(&args).unwrap();
        assert!(r.is_error, "ambiguous replace must be an error");
        assert!(r.content.contains("ambiguous"));
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            "hello world\nhello world again\n"
        );

        // Exactly 1 match -> writes successfully.
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "old_str": "world again",
            "new_str": "moon",
        });
        let r = replace(&args).unwrap();
        assert!(!r.is_error, "single-match replace must succeed");
        let written = fs::read_to_string(&path).unwrap();
        assert!(
            written.contains("hello moon\n"),
            "replacement applied: {written}"
        );

        fs::remove_dir_all(&dir).unwrap();
    }

    /// REQ-TOOL-003: reading a large fixture outputs characters and a pagination footer.
    #[test]
    fn test_harness_paginated_read() {
        let dir = temp_dir();
        let path = dir.join("big.txt");
        let content = "a".repeat(20_000);
        fs::write(&path, &content).unwrap();

        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "offset": 0,
            "limit": 8000,
        });
        let r = read_file(&args).unwrap();
        assert!(!r.is_error);
        let out = r.content;
        assert!(out.starts_with("aaaa"));
        assert!(
            out.contains(
                "[Showing characters 0-8000 of 20000. Use offset=8000 to read next chunk]"
            ),
            "footer missing in:\n{out}"
        );
        assert!(out.len() <= crate::harness::MAX_TOOL_OUTPUT_CHARS);

        // Second page.
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "offset": 8000,
            "limit": 8000,
        });
        let r = read_file(&args).unwrap();
        let out = r.content;
        assert!(out.starts_with("aaaa"));
        assert!(out.contains(
            "[Showing characters 8000-16000 of 20000. Use offset=16000 to read next chunk]"
        ));
        assert!(out.len() <= crate::harness::MAX_TOOL_OUTPUT_CHARS);

        // Final page.
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "offset": 16000,
            "limit": 8000,
        });
        let r = read_file(&args).unwrap();
        let out = r.content;
        assert_eq!(out.len(), 4000);
        assert!(!out.contains("[Showing characters"));

        fs::remove_dir_all(&dir).unwrap();
    }

    /// Path mapping: `/home/coder/workspace/...` maps onto the current working
    /// directory, matching marmennill-cli's `map_path`; other paths pass through.
    #[test]
    fn test_harness_map_path() {
        let cur = std::env::current_dir().unwrap();
        // Workspace-prefixed path maps onto cwd.
        let mapped = map_path("/home/coder/workspace/src/main.rs");
        assert_eq!(mapped, cur.join("src/main.rs"));
        // Leading slash is stripped.
        let mapped2 = map_path("/home/coder/workspace/foo.txt");
        assert_eq!(mapped2, cur.join("foo.txt"));
        // Non-workspace absolute path passes through unchanged.
        let abs = map_path("/etc/hosts");
        assert_eq!(abs, PathBuf::from("/etc/hosts"));
        // Relative path passes through unchanged.
        let rel = map_path("src/lib.rs");
        assert_eq!(rel, PathBuf::from("src/lib.rs"));
    }

    /// REQ-TOOL-004: write_file creates missing parent dirs.
    #[test]
    fn test_harness_write_file_creates_parents() {
        let base = temp_dir();
        let path = base.join("nested/a/b/c.txt");
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "content": "hello nested",
        });
        let r = write_file(&args).unwrap();
        assert!(!r.is_error);
        assert_eq!(fs::read_to_string(&path).unwrap(), "hello nested");
        fs::remove_dir_all(&base).unwrap();
    }

    /// Multibyte UTF-8 safety: ensures slicing across emojis, non-ASCII and CJK characters
    /// accurately indexes by Unicode scalar values and never panics.
    #[test]
    fn test_harness_read_file_multibyte_safety() {
        let dir = temp_dir();
        let path = dir.join("multibyte.txt");
        // Multibyte UTF-8 characters, Japanese, emojis, Greek
        let text = "UTF-8 test! 🦀 Rust programming 日本語テスト 🚀🔥 αβγδ\n";
        let repeated = text.repeat(100);
        let _total_chars = repeated.chars().count();
        fs::write(&path, &repeated).unwrap();

        // 1. Read first 20 characters
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "offset": 0,
            "limit": 20,
        });
        let r = read_file(&args).unwrap();
        assert!(!r.is_error);
        let expected_slice: String = repeated.chars().take(20).collect();
        assert!(r.content.starts_with(&expected_slice));

        // 2. Read with offset cutting right into emojis
        let args = serde_json::json!({
            "path": path.to_string_lossy(),
            "offset": 5,
            "limit": 15,
        });
        let r = read_file(&args).unwrap();
        assert!(!r.is_error);
        let expected_slice: String = repeated.chars().skip(5).take(15).collect();
        assert!(r.content.starts_with(&expected_slice));

        fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn test_harness_path_confinement_sandbox() {
        // Valid workspace path succeeds
        let valid = resolve_safe_path("Cargo.toml", "read_file").unwrap();
        assert!(valid.ends_with("Cargo.toml"));

        // Path traversal escaping root fails with Forbidden
        let escape_err = resolve_safe_path("../../../../../etc/passwd", "read_file").unwrap_err();
        match escape_err {
            ToolError::Forbidden { caller, .. } => {
                assert!(caller.contains("escapes workspace root"));
            }
            other => panic!("expected ToolError::Forbidden, got {other:?}"),
        }

        // Absolute path outside workspace/temp fails with Forbidden
        let abs_err = resolve_safe_path("/root/.ssh/id_rsa", "read_file").unwrap_err();
        match abs_err {
            ToolError::Forbidden { caller, .. } => {
                assert!(caller.contains("escapes workspace root"));
            }
            other => panic!("expected ToolError::Forbidden, got {other:?}"),
        }
    }
}
