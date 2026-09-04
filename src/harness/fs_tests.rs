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
        out.contains("[Showing characters 0-8000 of 20000. Use offset=8000 to read next chunk]"),
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
    assert!(
        out.contains(
            "[Showing characters 8000-16000 of 20000. Use offset=16000 to read next chunk]"
        )
    );
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
#[cfg_attr(windows, ignore)]
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

#[test]
fn test_harness_write_file_path_inference_and_error() {
    let dir = temp_dir();
    let target = dir.join("test_inferred.md");

    // 1. When path is omitted but content mentions `saved to \`path\``
    let args_inferred = serde_json::json!({
        "content": format!("# Report\n\nsaved to `{}`\nDone.", target.to_string_lossy()),
    });
    let res = write_file(&args_inferred).expect("inferred path write succeeds");
    assert!(!res.is_error);
    assert!(target.exists());

    // 2. When path is completely missing and no heuristic matches
    let args_missing = serde_json::json!({
        "content": "Just content with no file hint",
    });
    let err = write_file(&args_missing).expect_err("missing path should fail");
    match err {
        ToolError::BadArguments { detail, .. } => {
            assert!(detail.contains("Specify the target file path"));
        }
        other => panic!("expected BadArguments, got {other:?}"),
    }

    fs::remove_dir_all(&dir).unwrap();
}
