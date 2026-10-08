//! Search tools: grep_search (gitignore-aware) and glob (sorted, capped).
//!
//! REQ-TOOL-005: `grep_search` uses the `ignore` crate to traverse files,
//! honoring `.gitignore` rules. Results are capped at 500 (default 100).
//! REQ-TOOL-006: `glob` returns sorted relative paths capped at 500 entries.
//!
//! # Truncation contract (recon bugs H8 and H10)
//!
//! Both tools used to cut their list **silently**: `grep_search` returned the
//! moment `max_results` was reached, and `glob` collected every match, sorted
//! them, then dropped the tail with `truncate(500)`. A caller could not tell a
//! complete answer from a partial one, so a subagent that grepped `TODO`,
//! received 100 of 900 lines, reported "all fixed" after a partial refactor.
//!
//! The contract now — identical for both tools:
//!
//! 1. **The walk always completes**, so `total_matches` is an *exact* count of
//!    what the pattern matched, not a lower bound. Only the first
//!    `max_results` entries are materialised, so memory stays bounded however
//!    many matches exist.
//! 2. **Truncation is machine-visible.** When more matches exist than were
//!    returned, the output gets a footer opening with
//!    [`GREP_TRUNCATION_MARKER`] / [`GLOB_TRUNCATION_MARKER`] followed by
//!    `truncated: true`, the returned/total/hidden counts, the effective
//!    `max_results` (plus `max_results_requested` when the caller asked for
//!    more than the hard cap allows), the ordering guarantee and a
//!    "narrow the search / raise `max_results`" hint.
//! 3. **Truncation is deterministic.** The shared [`search_walker`] installs the
//!    `ignore` crate's path sorter, so traversal is depth-first pre-order with
//!    sibling entries sorted lexicographically by path. `glob` additionally
//!    sorts its root-relative paths lexicographically and keeps the *first*
//!    `max_results` of that sorted set, so the same pattern on the same tree
//!    yields byte-identical output across runs.
//! 4. **An honest empty answer.** Zero matches still returns the literal
//!    `no matches` — and because the walk now completes, that string provably
//!    means "the whole tree was walked and nothing matched".
//! 5. **Byte-for-byte compatibility.** When nothing was clipped the output is
//!    exactly what these tools always produced: match lines joined by `\n` for
//!    `grep_search`, root-relative paths joined by `\n` for `glob`.
//!
//! # Glob grammar
//!
//! `*` matches within one path segment, `**` crosses path separators, `?`
//! matches one character, `\X` matches `X` literally (so `\{` matches a real
//! brace). Character classes (`[...]`) are **not** supported and are matched
//! literally.
//!
//! Brace alternation is supported (recon bug H10: `src/**/*.{rs,md}` used to be
//! escaped into a literal and answer "no matches" with total confidence). The
//! pattern is expanded into its alternative forms *before* matching, all
//! alternatives are tested against a single walk, and the union is de-duplicated
//! — see [`expand_braces`]. Nesting is supported up to
//! [`MAX_GLOB_BRACE_DEPTH`] levels and the whole pattern may expand to at most
//! [`MAX_GLOB_ALTERNATIVES`] alternatives. Anything that cannot be honoured is
//! rejected as `BadArguments`, never as a silent empty result.

use crate::harness::fs::{str_arg, usize_arg};
use crate::harness::{ToolError, ToolResult};
use crate::tool_names::{TOOL_GLOB, TOOL_GREP_SEARCH};
use ignore::WalkBuilder;
use regex::Regex;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::Path;

/// Hard cap on the number of grep results (REQ-TOOL-005).
pub const GREP_HARD_CAP: usize = 500;
/// Hard cap on the number of glob results (REQ-TOOL-006).
pub const GLOB_HARD_CAP: usize = 500;

/// Machine-visible prefix of the footer `grep_search` appends when its cap
/// clipped the list (recon bug H8). Callers and tests key off this constant
/// rather than re-wording the human-readable lines that follow it.
pub const GREP_TRUNCATION_MARKER: &str = "[grep_search truncated]";
/// Machine-visible prefix of the footer `glob` appends when its cap clipped the
/// list (recon bug H10).
pub const GLOB_TRUNCATION_MARKER: &str = "[glob truncated]";

/// Ceiling on how many alternative patterns one brace-heavy glob may grow into.
/// Past this the pattern is rejected instead of silently truncated — an
/// unbounded expansion would otherwise multiply a full tree walk per
/// alternative.
pub const MAX_GLOB_ALTERNATIVES: usize = 64;
/// Ceiling on brace-group nesting depth, so pathological patterns are rejected
/// with an explicit error rather than recursing without end.
pub const MAX_GLOB_BRACE_DEPTH: usize = 8;

/// The ordering guarantee, stated verbatim in every truncation footer.
const ORDERING_NOTE: &str = "ordering: depth-first pre-order walk with sibling entries sorted lexicographically by path; .gitignore honored";

/// `grep_search(pattern, path, max_results)` — regex search over files.
///
/// Walks `path` with the `ignore` crate (which honors `.gitignore`), reads each
/// file, and collects lines matching the regex. Returns at most `max_results`
/// match lines, hard-capped at [`GREP_HARD_CAP`].
///
/// The walk is *not* short-circuited when the cap is reached: the count keeps
/// climbing so the footer can state the exact `total_matches`. When the cap bit,
/// the footer described in the module docs is appended; the match lines
/// themselves are byte-identical to the previous output.
pub fn grep_search(args: &Value) -> Result<ToolResult, ToolError> {
    let pattern = str_arg(args, "pattern", TOOL_GREP_SEARCH)?;
    let raw_root = args.get("path").and_then(Value::as_str).unwrap_or(".");
    let safe_root = crate::harness::fs::resolve_safe_path(raw_root, TOOL_GREP_SEARCH)?;
    let requested = usize_arg(args, "max_results", 100, TOOL_GREP_SEARCH)?;
    let max_results = requested.min(GREP_HARD_CAP);

    let re = Regex::new(pattern).map_err(|e| ToolError::BadArguments {
        tool: TOOL_GREP_SEARCH.into(),
        detail: format!("invalid regex: {e}"),
    })?;

    let (results, total_matches) = grep_matches(&re, &safe_root, max_results);
    if total_matches == 0 {
        return Ok(ToolResult::ok("no matches"));
    }

    let mut content = results.join("\n");
    if total_matches > results.len() {
        append_footer(
            &mut content,
            &grep_truncation_footer(results.len(), total_matches, requested, max_results),
        );
    }
    Ok(ToolResult::ok(content))
}

/// Walk `root`, materialising at most `max_results` match lines while counting
/// **every** match — the count is what makes truncation visible to the caller.
fn grep_matches(re: &Regex, root: &Path, max_results: usize) -> (Vec<String>, usize) {
    let mut results: Vec<String> = Vec::new();
    let mut total_matches: usize = 0;
    for entry in search_walker(root).build().flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        if let Ok(content) = std::fs::read_to_string(path) {
            for (idx, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    total_matches += 1;
                    if results.len() < max_results {
                        results.push(format!("{}:{}: {}", path.display(), idx + 1, line));
                    }
                }
            }
        }
    }
    (results, total_matches)
}

/// The one walker both tools share: the `ignore` traversal that honors
/// `.gitignore`, with an explicit sorter so the order — and therefore which
/// entries survive a cap — is reproducible from run to run.
fn search_walker(root: &Path) -> WalkBuilder {
    let mut builder = WalkBuilder::new(root);
    builder
        .require_git(false)
        .sort_by_file_path(|a, b| a.cmp(b));
    builder
}

/// Build the machine-visible footer for a grep result set clipped by
/// `max_results`.
fn grep_truncation_footer(
    returned: usize,
    total_matches: usize,
    requested: usize,
    applied: usize,
) -> String {
    let hidden = total_matches - returned;
    let mut footer = format!(
        "\n\n{GREP_TRUNCATION_MARKER} truncated: true\n\
         returned_matches: {returned}\n\
         total_matches: {total_matches}\n\
         hidden_matches: {hidden}\n\
         max_results: {applied}\n"
    );
    if requested > applied {
        footer.push_str(&format!(
            "max_results_requested: {requested} clamped_to: {GREP_HARD_CAP}\n"
        ));
    }
    footer.push_str(&format!(
        "{ORDERING_NOTE}\n\
         hint: the list above is NOT complete — {hidden} more match(es) exist. Narrow the search with a more specific `pattern` or a narrower `path`, or raise `max_results` (max {GREP_HARD_CAP})."
    ));
    footer
}

/// One glob answer: the capped, sorted, de-duplicated paths plus the exact total
/// the full walk found.
#[derive(Debug)]
pub(crate) struct GlobOutcome {
    /// Root-relative paths, de-duplicated, sorted lexicographically, at most
    /// `max_results` long (the lexicographically first ones).
    pub(crate) matches: Vec<String>,
    /// Exact count of distinct files the pattern matched, capped list or not.
    pub(crate) total_matches: usize,
    /// How many alternative patterns the brace expansion produced (1 when the
    /// pattern contains no braces).
    pub(crate) alternatives: usize,
}

impl GlobOutcome {
    /// Whether [`GlobOutcome::matches`] is a partial view.
    fn truncated(&self) -> bool {
        self.total_matches > self.matches.len()
    }
}

/// `glob(pattern, max_results)` — filesystem glob expansion, sorted, capped.
///
/// Matches are relative paths. Root is the workspace root directory. Brace
/// alternation (`{a,b}`, nested included) is expanded before matching; a
/// malformed brace group is an explicit error, never an empty result.
pub fn glob(args: &Value) -> Result<ToolResult, ToolError> {
    let pattern = str_arg(args, "pattern", TOOL_GLOB)?;
    let requested = usize_arg(args, "max_results", GLOB_HARD_CAP, TOOL_GLOB)?;
    let max_results = requested.min(GLOB_HARD_CAP);
    let root = crate::harness::get_workspace_root();
    let outcome = glob_in_root(pattern, &root, max_results)?;

    if outcome.total_matches == 0 {
        return Ok(ToolResult::ok("no matches"));
    }

    let mut content = outcome.matches.join("\n");
    if outcome.truncated() {
        append_footer(
            &mut content,
            &glob_truncation_footer(
                outcome.matches.len(),
                outcome.total_matches,
                requested,
                max_results,
                outcome.alternatives,
            ),
        );
    }
    Ok(ToolResult::ok(content))
}

/// Glob expansion rooted at `root`, returning root-relative paths that are
/// de-duplicated and sorted lexicographically, keeping the first `max_results`
/// of that sorted set while counting every distinct match found.
///
/// Returns `BadArguments` when the pattern cannot be honoured (unbalanced or
/// empty brace group, too many alternatives, over-nested braces, a regex the
/// pattern compiles to) — see the module docs for the grammar.
pub(crate) fn glob_in_root(
    pattern: &str,
    root: &Path,
    max_results: usize,
) -> Result<GlobOutcome, ToolError> {
    let alternatives = match expand_braces(pattern) {
        Ok(alternatives) => alternatives,
        Err(detail) => {
            return Err(ToolError::BadArguments {
                tool: TOOL_GLOB.into(),
                detail,
            });
        }
    };
    let mut regexes: Vec<Regex> = Vec::with_capacity(alternatives.len());
    for alt in &alternatives {
        match glob_to_regex(alt) {
            Ok(re) => regexes.push(re),
            Err(e) => {
                return Err(ToolError::BadArguments {
                    tool: TOOL_GLOB.into(),
                    detail: format!("invalid glob pattern '{alt}': {e}"),
                });
            }
        }
    }

    // A BTreeSet gives de-duplication and lexicographic order for free; the
    // `pop_last` above the cap keeps the lexicographically first `max_results`
    // paths instead of buffering the whole match set.
    let mut matched: BTreeSet<String> = BTreeSet::new();
    let mut total_matches: usize = 0;
    for entry in search_walker(root).build().flatten() {
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
        if regexes.iter().any(|re| re.is_match(&rel)) {
            total_matches += 1;
            matched.insert(rel);
            if matched.len() > max_results {
                matched.pop_last();
            }
        }
    }

    Ok(GlobOutcome {
        matches: matched.into_iter().collect(),
        total_matches,
        alternatives: alternatives.len(),
    })
}

/// Build the machine-visible footer for a glob result set clipped by the cap.
fn glob_truncation_footer(
    returned: usize,
    total_matches: usize,
    requested: usize,
    applied: usize,
    alternatives: usize,
) -> String {
    let hidden = total_matches - returned;
    let mut footer = format!(
        "\n\n{GLOB_TRUNCATION_MARKER} truncated: true\n\
         returned_paths: {returned}\n\
         total_matches: {total_matches}\n\
         hidden_matches: {hidden}\n\
         max_results: {applied}\n\
         pattern_alternatives: {alternatives}\n"
    );
    if requested > applied {
        footer.push_str(&format!(
            "max_results_requested: {requested} clamped_to: {GLOB_HARD_CAP}\n"
        ));
    }
    footer.push_str(&format!(
        "{ORDERING_NOTE}; returned paths are the lexicographically first {applied} of {total_matches}\n\
         hint: the list above is NOT complete — {hidden} more match(es) exist. Narrow the pattern (add a directory prefix, or pin the extension with a brace group such as `*.rs`), or raise `max_results` (max {GLOB_HARD_CAP})."
    ));
    footer
}

/// Append a truncation footer without leaving a stray blank lead when the tool
/// returned no entries at all (e.g. `max_results: 0`).
fn append_footer(content: &mut String, footer: &str) {
    if content.is_empty() {
        content.push_str(footer.trim_start());
    } else {
        content.push_str(footer);
    }
}

/// Expand the brace alternation of `pattern` into the list of alternative
/// patterns it stands for.
///
/// * `src/*.{rs,md}` → `["src/*.rs", "src/*.md"]`
/// * `{a,{b,c}}` → `["a", "b", "c"]` (nesting is expanded, not treated as
///   literal text; alternatives come back de-duplicated in left-to-right
///   expansion order)
/// * `{a}` → `["a"]` (a one-option group is still alternation, matching the
///   `globset` behaviour this hand-rolled expansion stands in for — `globset`
///   is only a transitive dependency of `ignore` and is not declared by this
///   crate, so no dependency was added)
/// * `\{a,b\}` → `["\\{a,b\\}"]` — backslash escapes keep a brace literal.
///
/// Errors (surfaced as `BadArguments`, never as an empty result): unbalanced
/// `{`, an empty group `{}`, deeper nesting than [`MAX_GLOB_BRACE_DEPTH`], or
/// more alternatives than [`MAX_GLOB_ALTERNATIVES`]. A stray `}` with no
/// matching `{` is literal text, matching shell/globset semantics.
pub(crate) fn expand_braces(pattern: &str) -> Result<Vec<String>, String> {
    let mut out: Vec<String> = Vec::new();
    expand_into(pattern, pattern, 0, &mut out)?;
    let mut seen: BTreeSet<String> = BTreeSet::new();
    out.retain(|alt| seen.insert(alt.clone()));
    Ok(out)
}

/// Recursive worker for [`expand_braces`]: find the first top-level brace group
/// in `current`, substitute each alternative, and recurse on the result. The
/// prefix before that group is brace-free by construction, so every recursion
/// step makes progress.
fn expand_into(
    current: &str,
    original: &str,
    depth: usize,
    out: &mut Vec<String>,
) -> Result<(), String> {
    let chars: Vec<char> = current.chars().collect();
    let Some(open) = find_open_brace(&chars) else {
        out.push(current.to_string());
        return Ok(());
    };
    let Some(close) = find_close_brace(&chars, open) else {
        return Err(format!(
            "malformed brace group: unbalanced '{{' in '{original}' — every '{{' needs a matching '}}' (write '\\{{' to match a literal brace)"
        ));
    };
    if depth >= MAX_GLOB_BRACE_DEPTH {
        return Err(format!(
            "unsupported pattern '{original}': brace groups nested more than {MAX_GLOB_BRACE_DEPTH} levels deep"
        ));
    }
    let body = &chars[open + 1..close];
    if body.is_empty() {
        return Err(format!(
            "malformed brace group: empty '{{}}' in '{original}'"
        ));
    }

    let prefix: String = chars[..open].iter().collect();
    let tail: String = chars[close + 1..].iter().collect();
    for alternative in split_alternatives(body) {
        let mut combined = prefix.clone();
        combined.push_str(&alternative);
        combined.push_str(&tail);
        expand_into(&combined, original, depth + 1, out)?;
        if out.len() > MAX_GLOB_ALTERNATIVES {
            return Err(format!(
                "unsupported pattern '{original}': brace expansion produces more than {MAX_GLOB_ALTERNATIVES} alternatives"
            ));
        }
    }
    Ok(())
}

/// Index of the first unescaped top-level `{`, or `None` when the pattern has
/// no brace group left. A `}` seen before any `{` is literal and skipped.
fn find_open_brace(chars: &[char]) -> Option<usize> {
    let mut idx = 0usize;
    while idx < chars.len() {
        match chars[idx] {
            '\\' => idx += 2,
            '{' => return Some(idx),
            _ => idx += 1,
        }
    }
    None
}

/// Index of the `}` closing the group opened at `open`, or `None` when the
/// pattern is unbalanced.
fn find_close_brace(chars: &[char], open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut idx = open + 1;
    while idx < chars.len() {
        match chars[idx] {
            '\\' => idx += 2,
            '{' => {
                depth += 1;
                idx += 1;
            }
            '}' if depth == 0 => return Some(idx),
            '}' => {
                depth -= 1;
                idx += 1;
            }
            _ => idx += 1,
        }
    }
    None
}

/// Split a brace group's body at its top-level commas (nesting and `\,` stay
/// inside their alternative). Always returns at least one alternative.
fn split_alternatives(body: &[char]) -> Vec<String> {
    let mut parts: Vec<String> = Vec::new();
    let mut current = String::new();
    let mut depth = 0usize;
    let mut idx = 0usize;
    while idx < body.len() {
        match body[idx] {
            '\\' if idx + 1 < body.len() => {
                current.push(body[idx]);
                current.push(body[idx + 1]);
                idx += 2;
            }
            '{' => {
                depth += 1;
                current.push('{');
                idx += 1;
            }
            '}' => {
                depth = depth.saturating_sub(1);
                current.push('}');
                idx += 1;
            }
            ',' if depth == 0 => {
                parts.push(std::mem::take(&mut current));
                idx += 1;
            }
            other => {
                current.push(other);
                idx += 1;
            }
        }
    }
    parts.push(current);
    parts
}

/// Translate a glob pattern into an anchored regex.
fn glob_to_regex(pattern: &str) -> Result<Regex, String> {
    let mut re = String::from("^");
    let mut chars = pattern.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\\' => {
                // `\X` matches the literal character X (so `\{` matches a brace).
                match chars.next() {
                    Some(escaped) => re.push_str(&regex::escape(&escaped.to_string())),
                    None => re.push_str("\\\\"),
                }
            }
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
    Regex::new(&re).map_err(|e| format!("pattern compiles to an invalid matcher: {e}"))
}

#[cfg(test)]
#[path = "search_tests.rs"]
mod tests;
