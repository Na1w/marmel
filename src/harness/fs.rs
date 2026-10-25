//! Filesystem tools: `read_file` (paginated, byte-capped), `replace` (strict),
//! `write_file`.
//!
//! [`replace`] writes only when the target string occurs exactly once — 0 or ≥2
//! matches return an error without touching the file. [`read_file`] is
//! character-paginated (`offset`, `limit`, `[Showing characters X-Y of Z...]`
//! footer) and never loads more than [`READ_FILE_MAX_BYTES`] from one file: a
//! larger file yields the requested window of the loaded head plus a
//! machine-visible [`READ_FILE_TRUNCATION_MARKER`] block (`truncated: true`,
//! byte/character offsets, re-run hint) instead of an opaque failure.
//! [`write_file`] auto-creates parent directories with mode 0o755.
//!
//! Both writers go through [`write_file_atomic`]: a per-caller unique staged name
//! created with `O_EXCL`, renamed within the same directory, and removed on
//! *every* failure path — including a failed `rename`. Concurrent `replace` calls
//! on one target are serialised per path so a later rename cannot silently
//! discard an earlier edit. Every path argument passes through `map_path`, which
//! maps the canonical container path `/home/coder/workspace/...` onto the current
//! working directory.

use crate::harness::{ToolError, ToolResult};
use crate::tool_names::{TOOL_READ_FILE, TOOL_REPLACE, TOOL_WRITE_FILE};
use serde_json::Value;
use std::ffi::{OsStr, OsString};
use std::io::Write;
use std::path::{Component, Path, PathBuf};
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

// ── Windows-safe spelling normalization & containment ─────────────────────
//
// `Path::starts_with` compares *components byte-exactly*: `Prefix::VerbatimDisk('C')`
// never equals `Prefix::Disk('C')`, and `RUNNER~1` never equals `runneradmin`
// (`Component` derives `PartialEq` over the raw `OsStr`; there is no case
// folding anywhere in `std`'s `Path`). On Windows the two operands of the
// containment test below are produced by *different* pipelines:
//
// * `std::fs::canonicalize` = `CreateFileW` + `GetFinalPathNameByHandleW
//   (VOLUME_NAME_DOS | FILE_NAME_NORMALIZED)` → a `\\?\` **verbatim** path with
//   every 8.3 short name expanded to its long name and the casing stored on disk;
// * `std::env::temp_dir()` = `GetTempPath2W`, which returns the `%TEMP%`
//   environment variable **verbatim as a string** — non-verbatim, and on
//   GitHub-hosted Windows runners routinely 8.3-shortened
//   (`C:\Users\RUNNER~1\AppData\Local\Temp`).
//
// Comparing one spelling against the other is a spelling-equality test masquerading
// as a containment test, which is what made a file that really is inside the temp
// root look like an escape on `windows-latest` only (docs/windows_ci_root_cause.md
// §1). Everything below exists so both sides of every containment decision — and
// every lock-shard key — is the *same* spelling-derived key.

/// Windows verbatim / device-namespace prefixes that `Path::canonicalize()` can
/// introduce (`\\?\`, `\\.\`, `\\localhost\`) plus the namespace spellings that
/// must never be mistaken for an ordinary volume path (`\\?\UNC\`,
/// `\\?\GLOBALROOT`), and their `/` spellings. Every entry is ASCII, so a prefix
/// match always ends on a `char` boundary.
///
/// Longest-first on purpose: [`strip_windows_path_prefix`] returns on the first
/// hit, so `\\?\UNC\server\share` is trimmed as one unit instead of leaving a
/// stray `UNC\` component behind.
pub(crate) const WINDOWS_PATH_PREFIXES: &[&str] = &[
    "\\\\?\\UNC\\",
    "\\\\?\\GLOBALROOT\\",
    "\\\\?\\",
    "\\\\.\\",
    "\\\\localhost\\",
    "//?/UNC/",
    "//?/GLOBALROOT/",
    "//?/",
    "//./",
    "//localhost/",
];

/// Drop a leading [`WINDOWS_PATH_PREFIXES`] entry, case-insensitively.
pub(crate) fn strip_windows_path_prefix(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    for prefix in WINDOWS_PATH_PREFIXES {
        let p = prefix.as_bytes();
        if bytes.len() >= p.len() && bytes[..p.len()].eq_ignore_ascii_case(p) {
            return &raw[p.len()..];
        }
    }
    raw
}

/// Which spelling rules apply to a path: the Windows rules (`\\?\` verbatim
/// prefixes, 8.3 short names, case- and separator-insensitive volumes) or the
/// Unix rules (the canonical spelling *is* the only spelling, byte-exact and
/// case-sensitive).
///
/// The containment helpers take this as a parameter instead of reading `cfg!`
/// inline so the **Windows** branch is unit-testable on a Linux runner — that is
/// the whole reason the Windows spelling bug could ship: nothing on Linux could
/// ever exercise the Windows comparison. Production always passes
/// [`PathFamily::current`], so live behaviour is exactly what the platform
/// requires; the parameterized entry points exist for tests.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum PathFamily {
    Unix,
    Windows,
}

impl PathFamily {
    /// The family of the platform this binary was **built** for — the value every
    /// production call site passes.
    pub(crate) const fn current() -> Self {
        if cfg!(windows) {
            PathFamily::Windows
        } else {
            PathFamily::Unix
        }
    }

    /// Windows volumes are ASCII-case-insensitive; Unix ones are case-sensitive.
    fn folds_case(self) -> bool {
        matches!(self, PathFamily::Windows)
    }
}

/// [`normalize_for_compare`] with an explicit [`PathFamily`] (see the type for why).
pub(crate) fn normalize_for_compare_in_family(raw: &str, family: PathFamily) -> String {
    if family == PathFamily::Unix {
        return raw.to_owned();
    }
    strip_windows_path_prefix(raw).replace('\\', "/")
}

/// Test-facing alias for [`normalize_for_compare_in_family`] pinned to the build
/// platform, kept because `src/harness/search_tests.rs` reuses exactly this shape
/// instead of carrying its own copy of the Windows spelling rules (one
/// implementation, no duplication). Production containment calls
/// [`normalize_for_compare_in_family`] directly with an explicit [`PathFamily`],
/// which is why this alias is compiled only for tests.
///
/// **No-op on Unix**: under [`PathFamily::current`] = [`PathFamily::Unix`] the
/// input is returned untouched, byte for byte, so a Unix run compares exactly the
/// strings it always compared.
#[cfg(test)]
pub(crate) fn normalize_for_compare(raw: &str) -> String {
    normalize_for_compare_in_family(raw, PathFamily::current())
}

/// The only spelling [`path_within`] and [`lock_shard_for`] are allowed to look
/// at: a platform-normalized comparison key derived from `path`.
///
/// **Unix: identity**, byte for byte — there the canonical spelling *is* the
/// unique spelling, so containment and sharding keep exactly the semantics they
/// always had.
///
/// **Windows**: strip the verbatim / device / namespace prefix, unify `\` and
/// `/`, uppercase the drive letter and ASCII-fold the rest of the case, so
/// `\\?\C:\Users\runneradmin\AppData\Local\Temp\f` (canonicalized) and
/// `c:/users/RUNNERADMIN/appdata/local/temp/F` (raw `%TEMP%` spelling, DOS vs
/// verbatim, any case) collapse onto one key. The key deliberately does **not**
/// expand 8.3 aliases (`RUNNER~1` ≠ `runneradmin`): that expansion is what
/// [`canonical_existing_prefix`] (i.e. `GetFinalPathNameByHandleW`) does, and the
/// gate feeds it **both** operands — the fold only removes spelling noise, so it
/// can never merge two different files. Device/namespace spellings survive as
/// plain relative-looking keys (`Device/HarddiskVolume1/…`, `PhysicalDrive0`),
/// which can never component-match a rooted key (`C:/…`) — so they stay denied.
///
/// The key is built from `to_string_lossy()` (like the pre-existing `hash_path`);
/// a path component that is not valid UTF-8 renders as `U+FFFD`, which can in
/// principle alias two distinct exotic components onto one key. Both containment
/// and sharding compare full component lists, so this is not a containment bypass
/// for any UTF-8 path.
pub(crate) fn containment_key(path: &Path) -> String {
    containment_key_in_family(path, PathFamily::current())
}

/// [`containment_key`] with an explicit [`PathFamily`], so the Windows spelling
/// rules are unit-testable on a Linux runner (this is the
/// `containment_key(&Path, PathFamily)` shape named in
/// `docs/windows_ci_root_cause.md` §RECOMMENDED FIX, split into two names because
/// production needs a zero-argument entry point). Production only ever calls
/// [`containment_key`].
pub(crate) fn containment_key_in_family(path: &Path, family: PathFamily) -> String {
    let raw = path.to_string_lossy();
    if family == PathFamily::Unix {
        return raw.into_owned();
    }
    let mut key = normalize_for_compare_in_family(&raw, family);
    // `x:` → `X:`. `normalize_for_compare_in_family` already removed any verbatim
    // prefix and unified separators, so a `:` at byte 1 can only be an ASCII drive
    // letter, and indices 0..2 are `char` boundaries.
    if key.as_bytes().get(1) == Some(&b':') {
        key[..1].make_ascii_uppercase();
        key[2..].make_ascii_lowercase();
    } else {
        key.make_ascii_lowercase();
    }
    key
}

/// Component list of a [`containment_key`]: the key has already unified `\` to
/// `/`, and empty pieces plus `.` carry no information. Splitting into components
/// (rather than taking a byte prefix) is what keeps the sibling-prefix bypass
/// dead: `/tmp/foobar` is **not** inside `/tmp/foo`.
fn key_components(key: &str) -> Vec<&str> {
    key.split('/')
        .filter(|part| !part.is_empty() && *part != ".")
        .collect()
}

/// Is `target` inside `root`, comparing **only** [`containment_key`] spellings,
/// component by component?
///
/// Argument order is **(anchor/allow-list entry, candidate)** — i.e.
/// `path_within(root, target)`, the reverse of a `(child, parent)` reading. The
/// parameter names are load-bearing: swapping them turns an allow into a deny.
///
/// Never `Path::starts_with` on operands that came from different normalization
/// pipelines: a `\\?\`-verbatim / long-name side can never prefix-match a raw
/// 8.3-short-name side (`Prefix::VerbatimDisk != Prefix::Disk`, byte-exact
/// component equality). Callers must therefore hand in operands that all went
/// through [`canonical_existing_prefix`].
pub(crate) fn path_within(root: &Path, target: &Path) -> bool {
    path_within_in_family(root, target, PathFamily::current())
}

/// [`path_within`] with an explicit [`PathFamily`], so the Windows spelling rules
/// (`\\?\` verbatim prefixes, drive letters, case- and separator-insensitive
/// volumes) are unit-testable on a Linux runner (the `path_within(…, family)`
/// shape named in `docs/windows_ci_root_cause.md` §RECOMMENDED FIX). Same
/// argument order: `path_within_in_family(root, target, family)`. Production
/// calls [`path_within`], which pins the family to the build platform.
pub(crate) fn path_within_in_family(root: &Path, target: &Path, family: PathFamily) -> bool {
    let root_key = containment_key_in_family(root, family);
    let target_key = containment_key_in_family(target, family);
    let root_parts = key_components(&root_key);
    let target_parts = key_components(&target_key);
    // Fail closed: an anchor with no components at all proves nothing — it must
    // not become a wildcard "everything is inside" allowance.
    if root_parts.is_empty() {
        return false;
    }
    // Fail closed on an operand that still carries a literal `..`. The gate feeds
    // this function only operands normalized by [`canonical_existing_prefix`],
    // whose [`join_normalized`] already folded `.` and `..` lexically, so a
    // surviving `..` means a caller skipped normalization. Refusing is the only
    // sound answer: a plain component-wise prefix match would happily accept
    // `C:\Temp\..\..\Windows\x` under `C:\Temp` (its *leading* components match),
    // and folding here would need anchor semantics this function does not own.
    // (`Vec<&str>::contains` is *component equality* on the split key, never the
    // substring `str::contains` — so it cannot weaken the component-wise rule.)
    if root_parts.contains(&"..") || target_parts.contains(&"..") {
        return false;
    }
    root_parts.len() <= target_parts.len()
        && root_parts
            .iter()
            .zip(target_parts.iter())
            // Windows file systems are case-insensitive and the key already
            // folded; folding again here is belt-and-braces and keeps the fold
            // *Windows-only*, because Unix file systems are case-sensitive and
            // Unix containment must stay byte-exact.
            .all(|(anchor, candidate)| {
                if family.folds_case() {
                    anchor.eq_ignore_ascii_case(candidate)
                } else {
                    anchor == candidate
                }
            })
}

/// Canonicalize the **deepest existing ancestor** of `path` and re-join the
/// missing tail onto it — and *never* fall back to a raw spelling.
///
/// Two silent raw fallbacks used to live here (`temp_dir().canonicalize()
/// .unwrap_or_else(|_| temp_dir())` and `cur.canonicalize().unwrap_or(cur)`), and
/// either one is enough to inject the raw 8.3/`%TEMP%` spelling into the
/// containment comparison and deny a legitimately allowed path. A path whose
/// deepest existing ancestor cannot be canonicalized is therefore an error: the
/// caller must deny rather than guess.
///
/// The walk is `try-canonicalize → one component up`, not `exists()` first:
/// `exists()` is a `stat` of a file that concurrent `replace` calls are renaming,
/// and on Windows `MoveFileExW(..., MOVEFILE_REPLACE_EXISTING)` leaves the target
/// transiently unresolvable (`ERROR_SHARING_VIOLATION` / delete-pending), so an
/// `exists()`-based branch is both a TOCTOU race and a needless syscall.
fn canonical_existing_prefix(tool: &str, path: &Path) -> Result<PathBuf, ToolError> {
    let components: Vec<Component<'_>> = path.components().collect();
    for split in (0..=components.len()).rev() {
        // The anchor must be non-empty, and the tail must never start with the
        // anchor's own `Prefix`/`RootDir`: re-joining past that point would
        // rebuild a *drive-relative* `C:foo` on Windows.
        if split == 0
            || matches!(
                components.get(split),
                Some(Component::RootDir) | Some(Component::Prefix(_))
            )
        {
            continue;
        }
        let anchor = components_to_path(&components[..split]);
        if let Ok(canonical) = anchor.canonicalize() {
            return Ok(join_normalized(&canonical, &components[split..]));
        }
    }
    Err(ToolError::BadArguments {
        tool: tool.to_string(),
        detail: format!(
            "cannot resolve an existing ancestor of '{}'",
            path.display()
        ),
    })
}

/// Rebuild a `PathBuf` from a leading slice of [`Component`]s (prefix, root and
/// normals in their original order).
fn components_to_path(components: &[Component<'_>]) -> PathBuf {
    let mut path = PathBuf::new();
    for component in components {
        path.push(component.as_os_str());
    }
    path
}

/// Re-join the missing tail of a path onto a canonicalized anchor, folding `.`
/// and `..` **lexically**.
///
/// The tail used to be appended verbatim, so a spelling such as
/// `<root>/newdir/../../outside` reached the containment test with literal `..`
/// components in it — and a component-wise prefix match still accepted it because
/// the *leading* components were inside the root. Folding makes that escape
/// visible to [`path_within`] instead of hiding it. `..` is clamped at the
/// anchor's own root: it can never pop the volume prefix or the root separator
/// off the anchor.
fn join_normalized(anchor: &Path, tail: &[Component<'_>]) -> PathBuf {
    let mut parts: Vec<OsString> = anchor
        .components()
        .map(|component| component.as_os_str().to_os_string())
        .collect();
    for component in tail {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match parts.last() {
                Some(last) if !is_anchor_component(last) => {
                    parts.pop();
                }
                // Nothing left to pop: stay put, [`path_within`] decides.
                _ => {}
            },
            other => parts.push(other.as_os_str().to_os_string()),
        }
    }
    let mut joined = PathBuf::new();
    for part in parts {
        joined.push(part);
    }
    joined
}

/// Is this component an anchor (`/`, `\`, a drive letter or a UNC/verbatim
/// prefix) that a `..` must never remove?
fn is_anchor_component(part: &OsStr) -> bool {
    part == OsStr::new("/")
        || part == OsStr::new("\\")
        || part == OsStr::new(".")
        || part.as_encoded_bytes().ends_with(b":")
}

/// The single deny wording used by [`resolve_safe_path`]: the message format is
/// part of the contract asserted by the tests, so it lives in one place.
fn escape_denied(path: &str, tool: &str) -> ToolError {
    ToolError::Forbidden {
        tool: tool.to_string(),
        caller: format!("access denied: path '{}' escapes workspace root", path),
    }
}

/// Resolve a path securely, ensuring it stays confined inside the workspace root
/// (or the process temp dir) — the **only** gate in front of the file tools.
///
/// Prevents path traversal attacks (e.g. `../../etc/passwd` or absolute escapes),
/// symlink/reparse escapes and Windows device/namespace escapes (`\\.\`,
/// `\\?\UNC\`, `\\?\GLOBALROOT`).
///
/// Every operand of the containment test — root, temp dir and target — goes
/// through the *same* [`canonical_existing_prefix`] pipeline (no raw fallback
/// anywhere: an anchor that cannot be normalized is a denial, never a raw
/// comparison), and the comparison is [`path_within`], which is spelling-
/// invariant (`\\?\`-verbatim vs DOS, long name vs 8.3 short name, `\` vs `/`,
/// drive-letter and ASCII case) instead of byte-exact `Path::starts_with`.
///
/// The returned `PathBuf` is that fully normalized form, so anything derived from
/// it — in particular the per-path lock shard in [`lock_shard_for`] — is the same
/// for every spelling of the same file.
pub fn resolve_safe_path(path: &str, tool: &str) -> Result<PathBuf, ToolError> {
    let raw_root = crate::harness::get_workspace_root();
    let canonical_root = canonical_existing_prefix(tool, &raw_root)?;
    // Exactly two allow-list entries, unchanged: the workspace root and
    // `std::env::temp_dir()`. A temp anchor that cannot be normalized is *absent*
    // from the allow-list for this call (fail closed) instead of being compared in
    // its raw `%TEMP%` spelling.
    let canonical_temp = canonical_existing_prefix(tool, &std::env::temp_dir()).ok();

    let raw_target = if path.starts_with(WORKSPACE_PREFIX) {
        let relative = path.strip_prefix(WORKSPACE_PREFIX).unwrap_or(path);
        let clean_relative = relative.strip_prefix('/').unwrap_or(relative);
        raw_root.join(clean_relative)
    } else if Path::new(path).is_absolute() {
        PathBuf::from(path)
    } else {
        raw_root.join(path)
    };

    let canonical_target = match canonical_existing_prefix(tool, &raw_target) {
        Ok(resolved) => resolved,
        // Fail closed: a path with no canonicalizable ancestor at all cannot be
        // proven inside either entry, so it is denied with the standard wording
        // (that is what keeps `\\?\GLOBALROOT\Device\…` and friends denied even
        // when the namespace refuses to canonicalize).
        Err(_) => return Err(escape_denied(path, tool)),
    };

    let within_temp = canonical_temp
        .as_ref()
        .is_some_and(|temp| path_within(temp, &canonical_target));
    if !path_within(&canonical_root, &canonical_target) && !within_temp {
        return Err(escape_denied(path, tool));
    }

    Ok(canonical_target)
}

pub const READ_FILE_MIN_LIMIT: usize = 2000;
pub const READ_FILE_MAX_LIMIT: usize = 8000;
pub const READ_FILE_DEFAULT_LIMIT: usize = 4000;

/// Hard ceiling on how many **bytes** [`read_file`] loads from a single file: an
/// uncapped `std::fs::read` on a log, a minified asset or a sparse file allocates
/// 2–3× the file size (`read` + `from_utf8_lossy` + `chars().count()`) and dumps it
/// straight into the model context.
///
/// **Why 256 KiB (262 144 bytes):** the session budget is
/// `config.max_context_tokens = 8192` by default (`src/config.rs`), and every tool
/// result is additionally clipped to `harness::MAX_TOOL_OUTPUT_CHARS = 10_000`
/// characters by `crate::harness::apply_tool_output_length_limit`. The widest page a
/// caller may request is `READ_FILE_MAX_LIMIT = 8000` characters, which
/// `crate::manager::context::count_text_tokens` (cl100k_base) prices at roughly
/// 2 000–3 000 tokens — already ~30% of the whole budget — so bytes beyond one page
/// window can never reach the model. 256 KiB = 32 × the widest page, and the worst
/// case per call is a quarter of a MiB of heap however large the file on disk is.
pub const READ_FILE_MAX_BYTES: usize = 256 * 1024;

/// Machine-visible prefix of the footer [`read_file`] appends when
/// [`READ_FILE_MAX_BYTES`] clipped the load. Callers and tests key off this
/// constant rather than re-wording the human-readable lines that follow it.
pub const READ_FILE_TRUNCATION_MARKER: &str = "[read_file truncated]";

/// `read_file(path, offset, limit)` — reads a UTF-8 file window by *characters*.
/// `offset` is a 0-based character index (default: 0); `limit` is a character count
/// (default: 4000, min: 2000, max: 8000), and values below 2000 are clamped to 2000 so
/// a caller cannot get trapped in micro-pagination. Sliced text comes back with a
/// pagination footer when more characters remain. Two-stage guard: at most
/// [`READ_FILE_MAX_BYTES`] bytes are read from disk and at most `READ_FILE_MAX_LIMIT`
/// characters are returned; when the byte ceiling bites the result is **not** an error
/// but the window plus a [`READ_FILE_TRUNCATION_MARKER`] block (`truncated: true`, byte
/// and character offsets, re-run hint), always cut on a character boundary
/// (see [`drop_partial_utf8_tail`]).
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
/// temp name nor lose each other's edit.
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

/// Number of process-local locks that serialise [`replace`] per target path.
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
    let shard = &REPLACE_LOCKS[lock_shard_for(path)];
    match shard.lock() {
        Ok(guard) => guard,
        // A panic somewhere else must not wedge every later write to this path.
        Err(poisoned) => poisoned.into_inner(),
    }
}

/// Pick the shard that serialises writers of `path`.
///
/// The key is [`containment_key`], never the raw spelling: `replace` calls that
/// address the *same file* through different spellings — 8.3 short name vs long
/// name, `\\?\`-verbatim vs DOS, different case or separators — must land on the
/// same shard, or the serialization above that stops concurrent edits from being
/// silently lost is bypassed by nothing more than how the caller typed the path.
/// `resolve_safe_path` already returns a normalized path, so on Unix this hashes
/// exactly the bytes it always hashed (identity key).
pub(crate) fn lock_shard_for(path: &Path) -> usize {
    let key = containment_key(path);
    hash_path(Path::new(&key)) % REPLACE_LOCK_SHARDS
}

/// FNV-1a over the path text; picks a lock shard deterministically.
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
/// Name shape: `.{stem}.tmp.{pid}.{nanos}.{seq:08}` — `pid` separates processes,
/// `nanos` separates calls a coarse clock would stamp alike, and `seq` is a
/// process-global atomic counter, so **no two calls in this process can ever
/// name the same path**, even with a one-second clock.
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
/// directory** followed by a `rename` (atomic within one filesystem). Guarantees:
/// * the staged name is unique per caller and created with `O_EXCL` — a concurrent
///   caller, or a leftover temp file, is never clobbered;
/// * readers of `path` only ever observe the old or the new content, never a
///   truncated or interleaved mix;
/// * any failure (payload write, permission copy, `rename`) removes the staged file,
///   so no orphan temp files survive;
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

#[cfg(test)]
#[path = "fs_path_tests.rs"]
mod path_tests;
