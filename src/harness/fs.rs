//! Filesystem tools: read_file (paginated, byte-capped), replace (strict),
//! write_file.
//!
//! REQ-TOOL-002: `replace` only writes when the target string occurs exactly
//! once — 0 or ≥2 matches return an error without touching the file.
//! REQ-TOOL-003: `read_file` is character-paginated with `offset` and `limit`
//! parameters and a `[Showing characters X-Y of Z...]` footer.
//! REQ-TOOL-004: `write_file` auto-creates parent directories with mode 0o755.
//!
//! Read ceiling (recon bug H2): [`read_file`] never loads more than
//! [`READ_FILE_MAX_BYTES`] from one file. A file larger than that yields the
//! requested window of the loaded head plus a machine-visible
//! [`READ_FILE_TRUNCATION_MARKER`] block (`truncated: true`, byte/character
//! offsets, and a re-run hint) instead of an opaque failure or an unbounded
//! allocation.
//!
//! Staged writes (recon bug H4): [`replace`] and [`write_file`] write through
//! [`write_file_atomic`], which stages into a name that is unique per caller
//! (`.{stem}.tmp.{pid}.{nanos}.{seq}`, created with `O_EXCL`), renames it within
//! the same directory (atomic, same filesystem), and removes the staged file on
//! *every* failure path — including a failed `rename`. Concurrent `replace`
//! calls on one target are additionally serialised per path so a later rename
//! cannot silently discard an earlier edit.
//!
//! Path mapping: every path argument is passed through `map_path`, which maps
//! the canonical container path `/home/coder/workspace/...` onto the current
//! working directory, matching marmennill-cli's local tool execution.

use crate::harness::{ToolError, ToolResult};
use crate::tool_names::{TOOL_READ_FILE, TOOL_REPLACE, TOOL_WRITE_FILE};
use serde_json::Value;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The canonical container workspace path prefix used by marmennill-cli.
pub const WORKSPACE_PREFIX: &str = "/home/coder/workspace";

/// Map a `/home/coder/workspace/...` path onto the current working directory,
/// matching marmennill-cli's `map_path`. Non-workspace paths are returned as-is.
pub fn map_path(path: &str) -> PathBuf {
    let cur_dir = crate::harness::get_workspace_root();
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
    let canonical_root = crate::harness::get_workspace_root();
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

pub const READ_FILE_MIN_LIMIT: usize = 2000;
pub const READ_FILE_MAX_LIMIT: usize = 8000;
pub const READ_FILE_DEFAULT_LIMIT: usize = 4000;

/// Hard ceiling on how many **bytes** [`read_file`] loads from a single file
/// (recon bug H2: an uncapped `std::fs::read` on a log, a minified asset or a
/// sparse file allocated 2–3× the file size — `read` + `from_utf8_lossy` +
/// `chars().count()` — and dumped it straight into the model context).
///
/// **Why 256 KiB (262 144 bytes):** the session budget is
/// `config.max_context_tokens = 8192` by default (`src/config.rs`), and every
/// tool result is additionally clipped to
/// `harness::MAX_TOOL_OUTPUT_CHARS = 10_000` characters by
/// `crate::harness::apply_tool_output_length_limit`. The widest page a caller
/// may request is `READ_FILE_MAX_LIMIT = 8000` characters, which
/// `crate::manager::context::count_text_tokens` (cl100k_base) prices at roughly
/// 2 000–3 000 tokens — already ~30% of the whole budget. Bytes beyond one page
/// window therefore can never reach the model, so loading more than 32 pages'
/// worth of source buys nothing while exposing the process to a multi-hundred-
/// MB allocation. 256 KiB = 32 × the widest page keeps ordinary source files
/// fully walkable in a handful of calls, while the worst case per call is a
/// quarter of a MiB of heap no matter how large the file on disk is.
pub const READ_FILE_MAX_BYTES: usize = 256 * 1024;

/// Machine-visible prefix of the footer [`read_file`] appends when
/// [`READ_FILE_MAX_BYTES`] clipped the load. Callers and tests key off this
/// constant rather than re-wording the human-readable lines that follow it.
pub const READ_FILE_TRUNCATION_MARKER: &str = "[read_file truncated]";

/// `read_file(path, offset, limit)` — reads a UTF-8 file window by *characters*.
///
/// - `offset` is 0-based character index (default: 0).
/// - `limit` is character count (default: 4000, min: 2000, max: 8000). Values
///   below 2000 are clamped to 2000 to prevent models getting trapped in micro-pagination loops.
/// - Returns sliced text with pagination footer if more characters remain.
/// - Two-stage guard: at most [`READ_FILE_MAX_BYTES`] bytes are read from disk,
///   and at most `READ_FILE_MAX_LIMIT` characters are returned. When the byte
///   ceiling bites, the result is **not** an error: the text is followed by a
///   [`READ_FILE_TRUNCATION_MARKER`] block carrying `truncated: true`, the byte
///   and character offsets, and a "re-run with offset/limit" hint. The cut is
///   always on a character boundary (see [`drop_partial_utf8_tail`]).
pub fn read_file(args: &Value) -> Result<ToolResult, ToolError> {
    let path = str_arg(args, "path", TOOL_READ_FILE)?;
    let offset = usize_arg(args, "offset", 0, TOOL_READ_FILE)?;
    let limit = usize_arg(args, "limit", READ_FILE_DEFAULT_LIMIT, TOOL_READ_FILE)?
        .clamp(READ_FILE_MIN_LIMIT, READ_FILE_MAX_LIMIT);

    let safe_path = resolve_safe_path(path, TOOL_READ_FILE)?;
    let file_bytes = std::fs::metadata(&safe_path)
        .map_err(anyhow::Error::from)?
        .len();
    // `len()` is only a hint (sparse/growing files): the ceiling is enforced by
    // the bounded read below, the metadata only decides which footer to emit.
    let over_cap = file_bytes > READ_FILE_MAX_BYTES as u64;

    let mut raw_bytes = if over_cap {
        read_head(&safe_path, READ_FILE_MAX_BYTES)?
    } else {
        std::fs::read(&safe_path).map_err(anyhow::Error::from)?
    };
    let bytes_read = raw_bytes.len();
    if over_cap {
        drop_partial_utf8_tail(&mut raw_bytes);
    }

    let content = String::from_utf8_lossy(&raw_bytes);
    let window_chars = content.chars().count();
    let start = offset.min(window_chars);
    let end = start.saturating_add(limit).min(window_chars);
    let mut out: String = content.chars().skip(start).take(end - start).collect();

    if over_cap {
        out.push_str(&read_truncation_footer(
            file_bytes,
            bytes_read,
            window_chars,
            start,
            end,
            offset,
            limit,
        ));
    } else if end < window_chars {
        out.push_str(&format!(
            "\n\n[Showing characters {start}-{end} of {window_chars}. Use offset={end} to read next chunk]"
        ));
    }

    Ok(ToolResult::ok(out))
}

/// Read at most `cap` bytes from the start of `path` in one bounded read.
///
/// The cap is enforced by the iterator itself, so a file whose `len()` lies (a
/// sparse file, or one being appended to concurrently) still cannot allocate
/// more than `cap` bytes.
fn read_head(path: &Path, cap: usize) -> Result<Vec<u8>, anyhow::Error> {
    use std::io::Read;
    let file = std::fs::File::open(path)?;
    let mut buf = Vec::with_capacity(cap);
    file.take(cap as u64).read_to_end(&mut buf)?;
    Ok(buf)
}

/// Length in bytes of the UTF-8 sequence started by the lead byte `lead`.
///
/// Anything that is not a well-formed lead byte (a stray continuation byte, or
/// the invalid `0xC0`/`0xC1`/`0xF5..` leads) reports `1` and is left to
/// `String::from_utf8_lossy`, which replaces it without shifting any offsets.
fn utf8_sequence_len(lead: u8) -> usize {
    if lead < 0x80 {
        1
    } else if lead & 0xE0 == 0xC0 {
        2
    } else if lead & 0xF0 == 0xE0 {
        3
    } else if lead & 0xF8 == 0xF0 {
        4
    } else {
        1
    }
}

/// Trim a trailing, incomplete UTF-8 sequence from `bytes` in place.
///
/// A byte cap can stop in the middle of a multi-byte character; decoding that
/// fragment would either split the character or fabricate a `U+FFFD`. The last
/// lead byte is at most 3 bytes behind the end of a complete sequence, so
/// scanning back 4 bytes is sufficient. Never panics on any input, including a
/// buffer that is nothing but continuation bytes.
fn drop_partial_utf8_tail(bytes: &mut Vec<u8>) {
    for back in 1..=bytes.len().min(4) {
        let lead = bytes[bytes.len() - back];
        if lead & 0xC0 == 0x80 {
            continue; // continuation byte — keep walking back
        }
        if utf8_sequence_len(lead) > back {
            bytes.truncate(bytes.len() - back);
        }
        return;
    }
    // No lead byte in the last four bytes at all: drop that run.
    let keep = bytes.len().saturating_sub(4);
    bytes.truncate(keep);
}

/// Build the machine-visible footer for a load that hit [`READ_FILE_MAX_BYTES`].
fn read_truncation_footer(
    file_bytes: u64,
    bytes_read: usize,
    window_chars: usize,
    start: usize,
    end: usize,
    offset: usize,
    limit: usize,
) -> String {
    let mut footer = format!(
        "\n\n{READ_FILE_TRUNCATION_MARKER} truncated: true\n\
         reason: file_bytes={file_bytes} exceeds read_cap_bytes={READ_FILE_MAX_BYTES}\n\
         file_bytes: {file_bytes}\n\
         read_cap_bytes: {READ_FILE_MAX_BYTES}\n\
         bytes_read: {bytes_read}\n\
         loaded_characters: {window_chars}\n\
         showing_characters: {start}-{end}\n"
    );
    if offset >= window_chars {
        footer.push_str(&format!(
            "requested_offset_beyond_loaded_head: true\n\
             hint: offset {offset} is past the {window_chars} characters that fit inside the read cap; re-run with a smaller offset (max {window_chars}) and limit={limit}, or use grep_search/glob to locate the section."
        ));
    } else {
        footer.push_str(&format!(
            "next_offset: {end}\n\
             hint: re-run with offset={end} limit={limit} to continue inside the loaded head; bytes past {bytes_read} were never read, so use grep_search/glob to reach sections further in."
        ));
    }
    footer
}

/// `replace(path, old_str, new_str)` — fails safely on 0 or ≥2 matches.
///
/// Counts occurrences of `old_str`. Only when exactly one match exists does it
/// perform the replacement; otherwise it returns an error and never writes.
///
/// The whole read-modify-write cycle is serialised per target path (see
/// [`replace_path_lock`]) and the write itself goes through
/// [`write_file_atomic`], so concurrent callers can neither collide on a staged
/// temp name nor lose each other's edit (recon bug H4).
pub fn replace(args: &Value) -> Result<ToolResult, ToolError> {
    let path = str_arg(args, "path", TOOL_REPLACE)?;
    let old_str = str_arg(args, "old_str", TOOL_REPLACE)?;
    let new_str = str_arg(args, "new_str", TOOL_REPLACE)?;

    let safe_path = resolve_safe_path(path, TOOL_REPLACE)?;
    let _target_guard = replace_path_lock(&safe_path);

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
    write_file_atomic(&safe_path, new_content.as_bytes())?;
    Ok(ToolResult::ok("replace applied"))
}

/// Number of process-local locks used to serialise [`replace`] per target path.
///
/// Sharded rather than one-lock-per-path: no map to grow, no eviction policy,
/// and the worst case is two unrelated files sharing a shard (harmless — the
/// critical section is a read plus an atomic write).
const REPLACE_LOCK_SHARDS: usize = 64;

static REPLACE_LOCKS: [std::sync::Mutex<()>; REPLACE_LOCK_SHARDS] =
    [const { std::sync::Mutex::new(()) }; REPLACE_LOCK_SHARDS];

/// Hold the shard guarding `path` for the duration of a `replace` cycle.
///
/// Without this, two concurrent `replace` calls on one target both read the
/// same base content, each stage a complete file, and the later `rename`
/// silently discards the earlier edit — an edit lost with no error.
fn replace_path_lock(path: &Path) -> std::sync::MutexGuard<'static, ()> {
    let shard = &REPLACE_LOCKS[hash_path(path) % REPLACE_LOCK_SHARDS];
    match shard.lock() {
        Ok(guard) => guard,
        // A panic somewhere else must not wedge every later write to this path.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// FNV-1a over the path text, used to pick a lock shard deterministically.
fn hash_path(path: &Path) -> usize {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in path.to_string_lossy().as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash as usize
}

/// Upper bound on how many characters of the target's file name are echoed into
/// the staged temp name, so the unique suffix can never push the name past
/// `NAME_MAX`.
const STAGING_NAME_MAX_STEM: usize = 48;

/// How many distinct names [`create_staging_file`] derives before giving up.
const STAGING_CREATE_ATTEMPTS: usize = 16;

/// Process-global monotonic counter mixed into every staged temp name.
static STAGING_SEQ: AtomicU64 = AtomicU64::new(0);

/// Stage a brand-new, exclusively-created file next to `target`.
///
/// Name shape: `.{stem}.tmp.{pid}.{nanos}.{seq:08}` where
/// * `pid` separates different processes,
/// * `nanos` separates calls that a coarse clock would otherwise stamp alike,
/// * `seq` is a process-global atomic counter, so **no two calls in this
///   process can ever name the same path**, even with a one-second clock.
///
/// The file is created with `create_new` (`O_EXCL`): if the name is already
/// taken — a stale temp file, a symlink planted by someone else, a genuine
/// race — the attempt is *not* clobbered, a fresh name is derived instead.
fn create_staging_file(
    dir: &Path,
    target: &Path,
) -> Result<(PathBuf, std::fs::File), anyhow::Error> {
    let stem = staging_stem(target);
    let pid = std::process::id();
    for _ in 0..STAGING_CREATE_ATTEMPTS {
        let seq = STAGING_SEQ.fetch_add(1, Ordering::Relaxed);
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or_default();
        let candidate = dir.join(format!(".{stem}.tmp.{pid}.{nanos}.{seq:08}"));
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&candidate)
        {
            Ok(file) => return Ok((candidate, file)),
            Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(err) => {
                return Err(anyhow::Error::from(err)
                    .context(format!("creating staging file in {}", dir.display())));
            }
        }
    }
    Err(anyhow::anyhow!(
        "could not derive a collision-free staging name for {} after {STAGING_CREATE_ATTEMPTS} attempts",
        target.display()
    ))
}

/// The `.`-prefixed stem echoed into a staged temp name: bounded in length
/// (char-boundary safe) and neutralised so it can never become a path
/// separator or an empty component.
fn staging_stem(target: &Path) -> String {
    let raw = target
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let clipped = crate::text_util::truncate_chars(&raw, STAGING_NAME_MAX_STEM);
    clipped
        .chars()
        .map(|c| {
            if c == '/' || c == '\\' || c == '\0' {
                '_'
            } else {
                c
            }
        })
        .collect()
}

/// Removes a staged temp file on every exit path unless [`StagedTemp::commit`]
/// disarms it — so a failed write, a failed `rename`, an early `?` or a panic
/// can never leave an orphan `.`-prefixed file behind.
struct StagedTemp {
    path: Option<PathBuf>,
}

impl StagedTemp {
    fn new(path: PathBuf) -> Self {
        Self { path: Some(path) }
    }

    /// The staged file has been renamed into place: stop cleaning up.
    fn commit(&mut self) {
        self.path = None;
    }
}

impl Drop for StagedTemp {
    fn drop(&mut self) {
        if let Some(path) = self.path.take()
            && let Err(err) = std::fs::remove_file(&path)
        {
            tracing::warn!("staging file {} was not removed: {err}", path.display());
        }
    }
}

/// Atomically (re)write `path` via a collision-proof staging file in the **same
/// directory** followed by a `rename` (atomic within one filesystem).
///
/// Guarantees:
/// * the staged name is unique per caller and created with `O_EXCL` — a
///   concurrent caller, or a leftover temp file, is never clobbered;
/// * readers of `path` only ever observe the old or the new content, never a
///   truncated or interleaved mix;
/// * any failure (payload write, permission copy, `rename`) removes the staged
///   file, so no orphan temp files survive;
/// * the mode of an existing `path` is preserved.
pub fn write_file_atomic(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    let dir = path
        .parent()
        .filter(|d| !d.as_os_str().is_empty())
        .unwrap_or(Path::new("."));

    let (staged_path, mut file) = create_staging_file(dir, path)?;
    let mut staged = StagedTemp::new(staged_path.clone());

    file.write_all(bytes).map_err(|err| {
        anyhow::Error::from(err).context(format!("staging write to {staged_path:?}"))
    })?;
    file.flush().map_err(|err| {
        anyhow::Error::from(err).context(format!("flushing staging file {staged_path:?}"))
    })?;
    drop(file);

    preserve_target_permissions(path, &staged_path)?;

    std::fs::rename(&staged_path, path).map_err(|err| {
        anyhow::Error::from(err).context(format!(
            "renaming staging file {} onto {}",
            staged_path.display(),
            path.display()
        ))
    })?;
    staged.commit();
    Ok(())
}

/// Copy the permissions of an existing `target` onto the staged replacement, so
/// an atomic rewrite never turns a `0o755` script into a `0o644` file. A brand
/// new file keeps the default mode the umask gives it.
#[cfg(unix)]
fn preserve_target_permissions(target: &Path, staged: &Path) -> Result<(), anyhow::Error> {
    match std::fs::metadata(target) {
        Ok(meta) => {
            std::fs::set_permissions(staged, meta.permissions()).map_err(anyhow::Error::from)
        }
        Err(_) => Ok(()),
    }
}

#[cfg(not(unix))]
fn preserve_target_permissions(_target: &Path, _staged: &Path) -> Result<(), anyhow::Error> {
    Ok(())
}

/// `write_file(path, content)` — writes full content to a path.
///
/// Creates any missing parent directories with permissions `0o755`. The write
/// itself goes through [`write_file_atomic`], so concurrent writers of the same
/// path can never produce a truncated or interleaved file and never leave a
/// staged temp file behind.
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
    write_file_atomic(&safe_path, content.as_bytes())?;
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
            // Heuristic fallback for write_file if LLM put "saved to `path`" in content
            if tool == TOOL_WRITE_FILE
                && let Some(content) = args.get("content").and_then(Value::as_str)
                && let Some(idx) = content.find("saved to `")
            {
                let sub = &content[idx + 10..];
                if let Some(end) = sub.find('`') {
                    let candidate = sub[..end].trim();
                    if !candidate.is_empty() && (candidate.contains('/') || candidate.contains('.'))
                    {
                        tracing::info!(
                            "write_file: inferred missing `path` ('{candidate}') from content text"
                        );
                        return Ok(candidate);
                    }
                }
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
                .or_else(|| args.get(crate::tool_names::TOOL_GLOB))
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
                .or_else(|| args.get(crate::tool_names::TOOL_REPLACE))
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
        detail: match key {
            "path" if tool == TOOL_WRITE_FILE => {
                "missing string field `path`. Specify the target file path in the tool arguments, e.g. {\"path\": \"docs/report.md\", \"content\": \"...\"}".to_string()
            }
            _ => format!("missing string field `{key}`"),
        },
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
#[path = "fs_tests.rs"]
mod tests;
