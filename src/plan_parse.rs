//! Single owner of the execution-plan **task-line grammar** — dedup cluster C3
//! (`docs/recon_duplication_helpers.md` §2.3).
//!
//! Before this module existed, the same "what is a plan task line?" rule was
//! open-coded five times, and the five copies had drifted apart:
//!
//! | site | grammar | checkbox | id forms | list markers |
//! |---|---|---|---|---|
//! | `manager::phase::TASK_LINE_RE` / `ALL_TASKS_RE` | `^\s*[-*]\s*\[\s*[ xX]?\s*\]\s*\*{0,2}\[?(t-…)\]?` | `[ ] [x] [X]` | `[t-001]`, bare `t-001` | `- *` |
//! | `manager::phase::check_off` | built per call: `^\s*(?:[-*]|\d+\.)\s*\[\s*\]\s*\*{0,2}\[?<tid>\]?\*{0,2}\b` | `[ ]` only | `[t-001]`, bare | `- * 1.` |
//! | `orchestrator::delegate` (checked-off guard + `brief_for_task`) | `^\s*-\s*\[\s*[ xX]?\s*\]\s*\[(t-…)\]\s*(.*)$` | `[ ] [x] [X]` | **`[t-001]` only** | `-` only |
//! | `orchestrator::plan_summary` | three loose regexes (`t-…` anywhere, `\[[xX]\]`, `\[\s*\]|\(\s*\)`) | `[x]` **but not `(x)`** | any `t-…` token anywhere | none (string trim) |
//! | `agents::prompt_builder::PLAN_TASK_RE` | phase's plus `\s*(.*?)$` | `[ ]` | `[t-001]`, bare | `- *` |
//! | `ui::tui::formatting` | checkbox + adjacent-token + bracketed fallback | `[ ] [x] [X] ( ) (x) (X)` | `[t-001]`, `(t-001)`, bare, bold, backtick | `- * + 1. 1)` |
//!
//! The drift was a real bug (`docs/recon_bugs_harness_llm_ui.md` §3 C1/C2): a
//! plan written as `- [ ] (t-002) migrate schema` was recognised by **no**
//! on-disk copy, so `pending_tasks()` listed nothing, `check_off("t-002")`
//! reported "task id not found or already checked", the plan could never reach
//! `is_complete()`, `archive()` refused forever, and the loop re-delegated the
//! same work indefinitely. Conversely `1. [ ] [t-004] x` was checkable but
//! invisible to the pending/all-task summaries.
//!
//! This module owns the grammar once: one task-line parser (checkbox state +
//! task id + byte/line offsets), one `check_off` line rewriter, and one
//! task-id token recogniser. `manager::phase` stays the authority for the
//! on-disk mutation (it owns the files and the `PLAN_MUTEX`); every other site
//! now asks this module what a task line is, so summaries, briefs, prompts and
//! the UI all agree with the disk.
//!
//! Deliberately out of scope (owned elsewhere, unchanged): the *marker* grammar
//! and the verdict that is allowed to flip a box — [`crate::markers::MissionMarker`],
//! applied to the plan by `manager::phase::Plan::check_plan_on_deliverable` —
//! and task-id **normalization** (`crate::task_id`, re-used here rather than
//! re-implemented). The deleted free-form success heuristic
//! (`output_is_success`, recon item M9) has no successor in this module: a box
//! is flipped only from an explicitly resolved task id, never from output prose.

use crate::task_id::normalize_task_id_ref;
use regex::Regex;
use std::sync::LazyLock;

/// Checkbox state of a plan task line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CheckboxState {
    /// `- [ ]`, `- []`, `- ( )` …
    Unchecked,
    /// `- [x]`, `- [X]`, `- (x)`, `- (X)` …
    Checked,
}

/// One parsed plan task line.
///
/// Spans are byte offsets **within `raw`**; `line_number` is the 0-based line
/// index within the document the line came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLine {
    /// Whether the checkbox is ticked.
    pub checkbox: CheckboxState,
    /// Task id of this line with `[`/`]`/`(`/`)`/`"`/`'` decoration stripped
    /// (via [`crate::task_id::normalize_task_id_ref`]), case preserved as spelled
    /// on disk. `None` for a checkbox line that carries no id.
    pub task_id: Option<String>,
    /// Byte span of the checkbox token (`[ ]`, `(x)`, …).
    pub checkbox_span: (usize, usize),
    /// Byte span of the bare task-id token (decoration such as `[`, `(`, `**`
    /// or backticks excluded), when present.
    pub id_span: Option<(usize, usize)>,
    /// Text after the task id, trimmed — exactly what the line says, including a
    /// trailing `(role)` hint. Used verbatim when building delegation briefs.
    pub description: String,
    /// [`TaskLine::description`] with the trailing `(role)` hint removed.
    pub description_body: String,
    /// Lower-cased trailing `(role)` token of the description, e.g.
    /// `(researcher)`, when present.
    pub role_hint: Option<String>,
    /// 0-based index of this line within the parsed document.
    pub line_number: usize,
    /// The original line, byte-for-byte.
    pub raw: String,
}

impl TaskLine {
    /// `true` when the line is an unchecked (pending) task.
    #[must_use]
    pub fn is_pending(&self) -> bool {
        self.checkbox == CheckboxState::Unchecked
    }

    /// `true` when the line is a checked (completed) task.
    #[must_use]
    pub fn is_completed(&self) -> bool {
        self.checkbox == CheckboxState::Checked
    }

    /// The description with its trailing `(role)` hint removed.
    #[must_use]
    pub fn description_without_role_hint(&self) -> &str {
        &self.description_body
    }

    /// This line rewritten with its checkbox ticked, preserving every other byte
    /// (notably the original task-id spelling such as `(t-002)`).
    /// `None` when the line is already checked.
    #[must_use]
    pub fn checked_line(&self) -> Option<String> {
        if self.is_completed() {
            return None;
        }
        let (start, end) = self.checkbox_span;
        let box_text = self.raw.get(start..end)?;
        let replacement = if box_text.starts_with('(') {
            "(x)"
        } else {
            "[x]"
        };
        let mut out = String::with_capacity(self.raw.len());
        out.push_str(&self.raw[..start]);
        out.push_str(replacement);
        out.push_str(&self.raw[end..]);
        Some(out)
    }
}

/// Head of a plan task line: optional indent / blockquote / list marker, then a
/// checkbox. Capture group 1 is the checkbox token itself.
///
/// Accepted markers: `-`, `*`, `+`, `1.`, `1)`. Accepted checkboxes: `[ ]`,
/// `[]`, `[x]`, `[X]`, `( )`, `(x)`, `(X)` (with arbitrary inner spaces). A
/// parenthesised *task id* such as `(t-002)` is never mistaken for a checkbox,
/// because the paren alternative only accepts an optional `x`/`X` inside.
static TASK_LINE_HEAD_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"^[ \t]*(?:>[ \t]*)*(?:[-*+]|\d+[.)])?[ \t]*(\[[ \t]*(?:[xX])?[ \t]*\]|\([ \t]*(?:[xX])?[ \t]*\))",
    )
    .expect("valid task-line head regex")
});

/// Task-id grammar: `t-` followed by `[A-Za-z0-9_-]+`, at a token boundary.
/// Group 1 is the id token. The only task-id token recogniser in the crate.
static TASK_ID_TOKEN_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[^A-Za-z0-9_-])(t-[A-Za-z0-9_-]+)").expect("valid task id token regex")
});

/// Bare task-id token (no boundary context), used to validate an adjacent token.
static TASK_ID_STRICT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^t-[A-Za-z0-9_-]+$").expect("valid strict task id regex"));

/// Whole-document unchecked-box scan (kept byte-identical to the historical
/// `manager::phase::UNCHECKED_BOX_RE`; `is_complete` semantics are not a
/// task-line grammar concern).
static UNCHECKED_BOX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\s*\]|\(\s*\)").expect("valid unchecked box regex"));

/// Whole-document checked-box scan (kept byte-identical to the historical
/// `manager::phase::CHECKED_BOX_RE`).
static CHECKED_BOX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[[xX]\]|\([xX]\)").expect("valid checked box regex"));

/// Decoration stripped from the front of a task id sitting after a checkbox:
/// markdown bold/italic, brackets, parentheses, backticks and quotes.
const ID_DECORATION: &[char] = &[' ', '\t', '[', '(', '*', '_', '`', '"', '\''];

/// `true` when `raw` is a bare task-id token (case-insensitive `t-…` grammar).
#[must_use]
pub fn is_task_id_token(raw: &str) -> bool {
    TASK_ID_STRICT_RE.is_match(raw)
}

/// Find the first task-id token in arbitrary text (marker bodies, worker tags,
/// descriptions). Returns the token exactly as written.
#[must_use]
pub fn find_task_id_token(text: &str) -> Option<String> {
    TASK_ID_TOKEN_RE
        .captures(text)
        .and_then(|caps| caps.get(1))
        .map(|m| m.as_str().to_string())
}

/// Case-insensitive, decoration-insensitive task-id comparison built on the
/// canonical [`crate::task_id`] normalizer.
#[must_use]
pub fn task_id_eq(a: &str, b: &str) -> bool {
    let left = normalize_task_id_ref(a);
    let right = normalize_task_id_ref(b);
    !left.is_empty() && !right.is_empty() && left.eq_ignore_ascii_case(right)
}

/// Parse one plan task line.
///
/// Returns `None` for anything that is not a task line: prose, headings, fenced
/// code, plain bullets without a checkbox, or a bare `(t-001)` with no checkbox.
/// A checkbox line that carries no task id parses to `Some` with
/// `task_id == None` (it is still a checklist item for progress accounting).
#[must_use]
pub fn parse_task_line(line: &str) -> Option<TaskLine> {
    parse_task_line_at(line, 0)
}

/// [`parse_task_line`] recording an explicit `line_number`.
#[must_use]
pub fn parse_task_line_at(line: &str, line_number: usize) -> Option<TaskLine> {
    let caps = TASK_LINE_HEAD_RE.captures(line)?;
    let box_match = caps.get(1)?;
    let box_start = box_match.start();
    let box_end = box_match.end();
    let box_text = &line[box_start..box_end];
    let inner = box_text
        .get(1..box_text.len().saturating_sub(1))
        .unwrap_or("");
    let checkbox = if inner.trim().eq_ignore_ascii_case("x") {
        CheckboxState::Checked
    } else {
        CheckboxState::Unchecked
    };

    let rest = &line[box_end..];
    let mut task_id = None;
    let mut id_span = None;
    let mut description = rest.trim();
    if let Some((rel_start, token, rel_end)) = leading_task_id_token(rest)
        && is_task_id_token(token)
    {
        let abs_start = box_end + rel_start;
        task_id = Some(normalize_task_id_ref(token).to_string());
        // Span of the bare id token itself (decoration such as `[`, `(`,
        // `**` or backticks excluded) — the exact text `task_id` came from.
        id_span = Some((abs_start, abs_start + token.len()));
        description = rest[rel_end..].trim();
    }

    let (description_body, role_hint) = split_role_hint(description);

    Some(TaskLine {
        checkbox,
        task_id,
        checkbox_span: (box_start, box_end),
        id_span,
        description: description.to_string(),
        description_body,
        role_hint,
        line_number,
        raw: line.to_string(),
    })
}

/// Parse every task line (checked **and** unchecked) of a plan document.
///
/// Lines sitting **inside a fenced code block** are skipped (recon item **L9**,
/// `docs/recon_bugs_manager.md`): a plan that documents its own
/// `- [ ] [t-xxx]` format inside a ```` ``` ```` block used to report phantom
/// pending task ids, which then showed up in `pending_tasks()`, in the
/// orchestrator's progress summary, in the delegation briefs — and could even be
/// delegated as if they were real work. The *line* grammar itself is unchanged
/// (see [`parse_task_line`]); only the document-level scan learned where fences
/// are.
#[must_use]
pub fn parse_tasks(text: &str) -> Vec<TaskLine> {
    scannable_lines(text)
        .filter_map(|(idx, line)| parse_task_line_at(line, idx))
        .collect()
}

/// A fenced-code delimiter line: ```` ``` ```` or `~~~` (three or more, only
/// leading whitespace before it). Returns the fence character and its length;
/// an opening fence may carry an info string (```` ```rust ````), a closing one
/// must be at least as long as the fence it closes and carries no info string.
fn fence_delimiter(line: &str) -> Option<(char, usize)> {
    let trimmed = line.trim_start();
    let first = trimmed.chars().next()?;
    if first != '`' && first != '~' {
        return None;
    }
    let run = trimmed.chars().take_while(|&c| c == first).count();
    if run < 3 {
        return None;
    }
    Some((first, run))
}

/// The lines of a plan document that are **not** inside a fenced code block,
/// paired with their original 0-based line index.
///
/// Deliberately simple (and stated so, because it is correctness relevant for
/// check-off): an unterminated fence suppresses the rest of the document, which
/// is what Markdown itself does. A closing fence must use the same character and
/// be at least as long as the opener. Fence delimiters themselves are reported
/// as *not scannable*.
fn scannable_lines(text: &str) -> impl Iterator<Item = (usize, &str)> {
    let mut kept = Vec::new();
    let mut state = FenceState::Outside;
    for (idx, line) in text.lines().enumerate() {
        match state {
            FenceState::Outside => match fence_delimiter(line) {
                Some((ch, len)) => state = FenceState::Inside { ch, len },
                None => kept.push((idx, line)),
            },
            FenceState::Inside { ch, len } => {
                if let Some((c, l)) = fence_delimiter(line)
                    && c == ch
                    && l >= len
                {
                    state = FenceState::Outside;
                }
            }
        }
    }
    kept.into_iter()
}

/// Fence state of [`scannable_lines`].
enum FenceState {
    Outside,
    Inside { ch: char, len: usize },
}

/// Task ids of all *unchecked* task lines, in document order, spelled as on disk.
/// This is `manager::phase::Plan::pending_tasks`.
#[must_use]
pub fn unchecked_task_ids(text: &str) -> Vec<String> {
    parse_tasks(text)
        .into_iter()
        .filter(|task| task.is_pending())
        .filter_map(|task| task.task_id)
        .collect()
}

/// Task ids of *all* task lines, in document order, spelled as on disk.
/// This is `manager::phase::Plan::all_tasks`.
#[must_use]
pub fn all_task_ids(text: &str) -> Vec<String> {
    parse_tasks(text)
        .into_iter()
        .filter_map(|task| task.task_id)
        .collect()
}

/// Unchecked task lines of a plan document.
#[must_use]
pub fn pending_lines(text: &str) -> Vec<TaskLine> {
    parse_tasks(text)
        .into_iter()
        .filter(|t| t.is_pending())
        .collect()
}

/// Checked task lines of a plan document.
#[must_use]
pub fn completed_lines(text: &str) -> Vec<TaskLine> {
    parse_tasks(text)
        .into_iter()
        .filter(|t| t.is_completed())
        .collect()
}

/// The task line owning `task_id`, if the plan mentions it.
#[must_use]
pub fn find_task_line(text: &str, task_id: &str) -> Option<TaskLine> {
    if normalize_task_id_ref(task_id).is_empty() {
        return None;
    }
    parse_tasks(text).into_iter().find(|task| {
        task.task_id
            .as_deref()
            .is_some_and(|id| task_id_eq(id, task_id))
    })
}

/// `true` when `task_id` appears in the plan as an unchecked task line.
#[must_use]
pub fn is_pending_task(text: &str, task_id: &str) -> bool {
    find_task_line(text, task_id).is_some_and(|task| task.is_pending())
}

/// `true` when `task_id` appears in the plan as a checked task line — the
/// re-delegation guard in `orchestrator::delegate`.
#[must_use]
pub fn is_checked_task(text: &str, task_id: &str) -> bool {
    find_task_line(text, task_id).is_some_and(|task| task.is_completed())
}

/// 0-based line index of the first unchecked task line, or `None`.
///
/// Line indices are absolute in the original document (the UI highlights that
/// exact line), and lines inside fenced code blocks are skipped — see
/// [`parse_tasks`] for the recon **L9** rationale.
#[must_use]
pub fn first_unchecked_line_index(text: &str) -> Option<usize> {
    scannable_lines(text).find_map(|(idx, line)| {
        parse_task_line_at(line, idx).filter(|task| task.is_pending())?;
        Some(idx)
    })
}

/// Flip the checkbox of a single line, but only when that line's own task id is
/// `task_id` and it is still unchecked. `None` otherwise — the caller then
/// leaves the line untouched.
///
/// The id must sit adjacent to the checkbox (any of the accepted decorations),
/// exactly like the on-disk authority: a task id that merely appears in another
/// task's description must never flip that other line.
#[must_use]
pub fn check_off_line(line: &str, task_id: &str) -> Option<String> {
    let task = parse_task_line(line)?;
    if task.is_completed() {
        return None;
    }
    let id = task.task_id.as_deref()?;
    if !task_id_eq(id, task_id) {
        return None;
    }
    task.checked_line()
}

/// Rewrite a whole plan document, ticking the first line whose own task id is
/// `task_id`. Returns the new content and whether anything was flipped — the
/// body of `manager::phase::Plan::check_off`.
///
/// **The rewrite is byte-faithful** (recon item **L6**,
/// `docs/recon_bugs_manager.md`): every line keeps its own terminator and the
/// document keeps (or keeps out) its trailing newline. The old
/// `content.lines().join("\n"` rewrite silently dropped the final newline and
/// turned every CRLF into LF as a *side effect of ticking one box*, so a plan
/// written with CRLF was re-written whole on the first check-off. Newline
/// normalization is a separate, explicit operation with exactly one owner:
/// [`normalize_newlines`].
///
/// Lines inside fenced code blocks are **not** candidates (recon **L9**): a plan
/// that quotes `- [ ] [t-001] …` as a format example inside a fence must not get
/// the *example* ticked instead of the real task line. The set of candidate
/// lines is exactly the set the readers use ([`parse_tasks`],
/// [`find_task_line`]), so what the orchestrator sees and what check-off writes
/// are the same line.
#[must_use]
pub fn check_off_content(content: &str, task_id: &str) -> (String, bool) {
    // Sorted absolute indices of the lines outside fences; `split_inclusive`
    // yields exactly one chunk per `lines()` entry, so the indices line up.
    let scannable: Vec<usize> = scannable_lines(content).map(|(idx, _)| idx).collect();
    let mut flipped = false;
    let mut updated = String::with_capacity(content.len());
    // `split_inclusive` keeps each terminator attached to its own line, which is
    // what makes the rewrite preserve CRLF and the trailing-newline state.
    for (idx, chunk) in content.split_inclusive('\n').enumerate() {
        let (body, eol) = match chunk.strip_suffix('\n') {
            Some(rest) => match rest.strip_suffix('\r') {
                Some(body) => (body, "\r\n"),
                None => (rest, "\n"),
            },
            None => (chunk, ""),
        };
        if !flipped
            && scannable.binary_search(&idx).is_ok()
            && let Some(rewritten) = check_off_line(body, task_id)
        {
            flipped = true;
            updated.push_str(&rewritten);
        } else {
            updated.push_str(body);
        }
        updated.push_str(eol);
    }
    if flipped {
        (updated, true)
    } else {
        // Nothing matched: hand back the input untouched so callers can tell
        // "no rewrite happened" apart from "rewritten and normalised".
        (content.to_string(), false)
    }
}

/// Normalize line endings of a plan document: `\r\n` and lone `\r` become `\n`.
///
/// This is the crate's **only** newline normalizer for plan text (recon L6). It
/// is applied exactly once — when the plan is written by
/// `manager::phase::Plan::create` — so every later read/scan/check-off sees one
/// spelling of end-of-line, and nothing else (not `check_off`, not the UI)
/// re-writes the file's line endings as a side effect.
///
/// A trailing newline is *not* added here: whether the document ends with one
/// is the writer's decision, and `check_off_content` preserves whatever was
/// chosen.
#[must_use]
pub fn normalize_newlines(text: &str) -> String {
    if !text.contains('\r') {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next();
            }
            out.push('\n');
        } else {
            out.push(ch);
        }
    }
    out
}

/// Task id belonging to a checklist line, lower-cased — the UI seam used by
/// `ui::tui::formatting::extract_plan_line_task_id`.
///
/// Resolution order: the id adjacent to the checkbox first (so
/// `- [ ] [t-100a1] Subtask (depends on t-100)` reports `t-100a1`, never
/// `t-100`), falling back to the first task-id token anywhere on the line.
/// Lines without a checkbox (headings, prose) yield `None`.
#[must_use]
pub fn task_id_of_line(line: &str) -> Option<String> {
    let task = parse_task_line(line)?;
    if let Some(id) = task.task_id.as_deref() {
        return Some(id.to_ascii_lowercase());
    }
    find_task_id_token(line).map(|id| normalize_task_id_ref(&id).to_ascii_lowercase())
}

/// `true` when `line` refers to `task_id` with strict boundary semantics: an
/// exact match against the line's own id when it has one (so `t-100` never
/// matches `t-100a1`), otherwise a boundary-safe substring search — the body of
/// `ui::tui::formatting::line_matches_task_id`.
#[must_use]
pub fn line_matches_task_id(line: &str, task_id: &str) -> bool {
    let clean_target = normalize_task_id_ref(task_id);
    if clean_target.is_empty() {
        return false;
    }
    let target_lower = clean_target.to_ascii_lowercase();

    if let Some(line_tid) = task_id_of_line(line) {
        return line_tid == target_lower;
    }

    let line_lower = line.to_ascii_lowercase();
    let mut search_start = 0usize;
    while let Some(pos) = line_lower[search_start..].find(&target_lower) {
        let abs_pos = search_start + pos;
        let before = if abs_pos == 0 {
            None
        } else {
            line_lower[..abs_pos].chars().last()
        };
        let after_pos = abs_pos + target_lower.len();
        let after = line_lower[after_pos..].chars().next();

        let before_ok = before
            .map(|c| !c.is_alphanumeric() && c != '-' && c != '_')
            .unwrap_or(true);
        let after_ok = after
            .map(|c| !c.is_alphanumeric() && c != '-' && c != '_')
            .unwrap_or(true);

        if before_ok && after_ok {
            return true;
        }
        search_start = abs_pos + target_lower.len();
        if search_start >= line_lower.len() {
            break;
        }
    }
    false
}

/// `true` when the text contains an unchecked checkbox anywhere (plan
/// completion gate).
///
/// Scanned line by line so that checkbox examples sitting **inside a fenced
/// code block** cannot hold a plan open forever (recon **L9**): a plan that
/// documents its own `- [ ]` template in a fence used to report
/// `has_unchecked_box == true` no matter how many real tasks were ticked, so
/// `Plan::is_complete` never fired and the plan never archived. The regex
/// grammar itself is unchanged (t-039); the only behavioural difference is that
/// `\s` inside a checkbox no longer matches across a line break, which is not a
/// spelling any writer produces.
#[must_use]
pub fn has_unchecked_box(text: &str) -> bool {
    scannable_lines(text).any(|(_, line)| UNCHECKED_BOX_RE.is_match(line))
}

/// `true` when the text contains a checked checkbox anywhere (plan completion
/// gate). Fence-aware for the same reason as [`has_unchecked_box`].
#[must_use]
pub fn has_checked_box(text: &str) -> bool {
    scannable_lines(text).any(|(_, line)| CHECKED_BOX_RE.is_match(line))
}

/// Strip a leading list marker / blockquote / table gutter from a plan line for
/// display (the closure `orchestrator::plan_summary` used to re-type twice).
#[must_use]
pub fn strip_list_marker(line: &str) -> &str {
    line.trim_start_matches(|c: char| {
        c == '-'
            || c == '*'
            || c == '+'
            || c == '>'
            || c == '|'
            || c == ' '
            || c == '.'
            || c.is_ascii_digit()
    })
    .trim()
}

/// Split a trailing `(role)` hint off a task description (`(researcher)`).
#[must_use]
pub fn split_role_hint(description: &str) -> (String, Option<String>) {
    let raw = description.trim();
    if let Some(open) = raw.rfind('(')
        && raw.ends_with(')')
    {
        let candidate = raw[open + 1..raw.len() - 1].trim().to_ascii_lowercase();
        return (raw[..open].trim().to_string(), Some(candidate));
    }
    (raw.to_string(), None)
}

/// Extract a task-id token sitting directly after a checkbox, skipping markdown
/// decoration. Returns the byte offset of the token (relative to the searched
/// text), the token itself, and the offset just past any decoration that closes
/// it (`[t-001]`, `**(t-002)**`, `` `t-003` `` …) so the description starts
/// where the prose does.
fn leading_task_id_token(text: &str) -> Option<(usize, &str, usize)> {
    let mut offset = 0usize;
    let mut openers = 0usize;
    for ch in text.chars() {
        match ch {
            '[' | '(' => {
                openers += 1;
                offset += ch.len_utf8();
            }
            _ if ID_DECORATION.contains(&ch) => offset += ch.len_utf8(),
            _ => break,
        }
    }
    let tail = &text[offset..];
    let len = tail
        .bytes()
        .take_while(|b| b.is_ascii_alphanumeric() || *b == b'-' || *b == b'_')
        .count();
    if len == 0 {
        return None;
    }
    let mut end = offset + len;
    let mut closers_left = openers;
    while let Some(ch) = text[end..].chars().next() {
        if (ch == ']' || ch == ')') && closers_left > 0 {
            closers_left -= 1;
            end += ch.len_utf8();
        } else if matches!(ch, '*' | '_' | '`' | '"' | '\'') {
            end += ch.len_utf8();
        } else {
            break;
        }
    }
    Some((offset, &text[offset..offset + len], end))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Table-driven acceptance: line -> (checkbox state, own task id).
    const ACCEPTED: &[(&str, CheckboxState, Option<&str>)] = &[
        (
            "- [ ] t-001 Build the parser",
            CheckboxState::Unchecked,
            Some("t-001"),
        ),
        (
            "- [x] t-001 Build the parser",
            CheckboxState::Checked,
            Some("t-001"),
        ),
        (
            "- [ ] [t-001] Build the parser",
            CheckboxState::Unchecked,
            Some("t-001"),
        ),
        (
            "- [x] [t-001] Build the parser",
            CheckboxState::Checked,
            Some("t-001"),
        ),
        (
            "- [ ] (t-002) Migrate schema",
            CheckboxState::Unchecked,
            Some("t-002"),
        ),
        (
            "- [x] (t-002) Migrate schema",
            CheckboxState::Checked,
            Some("t-002"),
        ),
        (
            "- [X] [t-003] Uppercase tick",
            CheckboxState::Checked,
            Some("t-003"),
        ),
        (
            "- [X] (t-020) Uppercase tick, paren id",
            CheckboxState::Checked,
            Some("t-020"),
        ),
        (
            "* [ ] [t-003] Star bullet",
            CheckboxState::Unchecked,
            Some("t-003"),
        ),
        (
            "+ [ ] (t-004) Plus bullet",
            CheckboxState::Unchecked,
            Some("t-004"),
        ),
        (
            "1. [ ] [t-005] Numbered bullet",
            CheckboxState::Unchecked,
            Some("t-005"),
        ),
        (
            "2) [ ] t-006 Paren numbered bullet",
            CheckboxState::Unchecked,
            Some("t-006"),
        ),
        (
            "  - [ ] [t-007] Indented",
            CheckboxState::Unchecked,
            Some("t-007"),
        ),
        (
            "\t- [ ] (t-008) Tab indented",
            CheckboxState::Unchecked,
            Some("t-008"),
        ),
        (
            "> - [ ] [t-009] Blockquoted",
            CheckboxState::Unchecked,
            Some("t-009"),
        ),
        (
            "- [ ] **[t-010]** Bold id",
            CheckboxState::Unchecked,
            Some("t-010"),
        ),
        (
            "- [ ] `t-011` Backticked id",
            CheckboxState::Unchecked,
            Some("t-011"),
        ),
        (
            "- [ ] \"t-012\" Quoted id",
            CheckboxState::Unchecked,
            Some("t-012"),
        ),
        (
            "- [ ] t-013: Colon id",
            CheckboxState::Unchecked,
            Some("t-013"),
        ),
        (
            "- [ ] **[T-014]** Uppercase spelling",
            CheckboxState::Unchecked,
            Some("T-014"),
        ),
        (
            "- [] t-015 Empty box",
            CheckboxState::Unchecked,
            Some("t-015"),
        ),
        (
            "- [  ]  t-016  Roomy box",
            CheckboxState::Unchecked,
            Some("t-016"),
        ),
        (
            "- [ x ] t-017 Roomy tick",
            CheckboxState::Checked,
            Some("t-017"),
        ),
        (
            "- ( ) (t-018) Paren checkbox",
            CheckboxState::Unchecked,
            Some("t-018"),
        ),
        (
            "- (X) (t-019) Paren checkbox ticked",
            CheckboxState::Checked,
            Some("t-019"),
        ),
        // A checkbox line with no id is still a checklist item, just not an id.
        ("- [ ] Task without an id", CheckboxState::Unchecked, None),
    ];

    /// Table-driven rejection: these are *not* plan task lines.
    const REJECTED: &[&str] = &[
        "",
        "   ",
        "# Execution Plan",
        "### Phase 1: Research",
        "- plain bullet with no checkbox",
        "* another plain bullet (t-001) in prose",
        "- [t-016] bracketed id but no checkbox",
        "(t-017) parenthesised id but no checkbox",
        "Prose mentioning [ ] and t-001 in the middle",
        "```",
        "| column | column |",
        "1. Numbered prose step",
    ];

    #[test]
    fn accepted_task_line_forms() {
        for (line, state, id) in ACCEPTED {
            let parsed =
                parse_task_line(line).unwrap_or_else(|| panic!("must be a task line: {line:?}"));
            assert_eq!(parsed.checkbox, *state, "checkbox state for {line:?}");
            assert_eq!(
                parsed.task_id.as_deref(),
                *id,
                "task id for {line:?} (decoration must be normalized away)"
            );
            assert_eq!(parsed.raw, *line, "raw line must be preserved: {line:?}");
        }
    }

    #[test]
    fn rejected_non_task_bullets() {
        for line in REJECTED {
            assert!(
                parse_task_line(line).is_none(),
                "must not be a task line: {line:?}"
            );
        }
    }

    #[test]
    fn parenthesised_id_survives_pending_and_all_task_scans() {
        let plan = "# Execution Plan\n- [ ] (t-002) Migrate schema\n- [x] [t-001] Baseline\n";
        assert_eq!(unchecked_task_ids(plan), vec!["t-002".to_string()]);
        assert_eq!(
            all_task_ids(plan),
            vec!["t-002".to_string(), "t-001".to_string()]
        );
        assert_eq!(first_unchecked_line_index(plan), Some(1));
    }

    #[test]
    fn check_off_line_preserves_original_id_spelling() {
        assert_eq!(
            check_off_line("- [ ] (t-002) migrate schema", "t-002").as_deref(),
            Some("- [x] (t-002) migrate schema")
        );
        assert_eq!(
            check_off_line("- [ ] (T-002) migrate schema", "t-002").as_deref(),
            Some("- [x] (T-002) migrate schema")
        );
        assert_eq!(
            check_off_line("- [ ] [t-001] Build", "[t-001]").as_deref(),
            Some("- [x] [t-001] Build")
        );
        assert_eq!(
            check_off_line("- ( ) (t-018) paren checkbox", "t-018").as_deref(),
            Some("- (x) (t-018) paren checkbox")
        );
        // Already checked, unknown id, or id only mentioned in the description:
        assert!(check_off_line("- [x] (t-002) done", "t-002").is_none());
        assert!(check_off_line("- [ ] (t-002) task", "t-999").is_none());
        assert!(check_off_line("- [ ] [t-011] report on (t-002)", "t-002").is_none());
        assert!(check_off_line("- [ ] [t-010] task", "t-01").is_none());
        assert!(check_off_line("- plain bullet (t-002)", "t-002").is_none());
    }

    #[test]
    fn check_off_content_flips_only_the_first_match() {
        let content = "- [ ] [t-010] Tenth\n- [ ] [t-01] First\n- [ ] [t-01] Again\n";
        let (updated, flipped) = check_off_content(content, "t-01");
        assert!(flipped);
        // Recon L6: the rewrite is byte-faithful — the trailing newline the
        // document had is still there, and only the one checkbox changed.
        assert_eq!(
            updated,
            "- [ ] [t-010] Tenth\n- [x] [t-01] First\n- [ ] [t-01] Again\n"
        );
        let (same, flipped) = check_off_content(content, "t-404");
        assert!(!flipped);
        assert_eq!(same, content);
    }

    /// Witness of the deleted newline handling (`content.lines().join("\n")`):
    /// ticking one box also dropped the final newline and flattened CRLF, i.e.
    /// the whole document was re-written as a side effect.
    fn legacy_lines_join_rewrite(content: &str, task_id: &str) -> String {
        content
            .lines()
            .map(|line| {
                if line.contains(&format!("[ ] [{task_id}]")) {
                    line.replace("[ ]", "[x]")
                } else {
                    line.to_string()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn check_off_content_preserves_line_endings_and_trailing_newline() {
        // CRLF document, no trailing newline.
        let crlf = "- [ ] [t-011] A\r\n- [ ] [t-012] B";
        let witness = legacy_lines_join_rewrite(crlf, "t-012");
        assert!(
            !witness.contains("\r\n") && !witness.ends_with('\n'),
            "witness must show the old rewrite flattening CRLF: {witness:?}"
        );

        let (updated, flipped) = check_off_content(crlf, "t-012");
        assert!(flipped);
        assert_eq!(updated, "- [ ] [t-011] A\r\n- [x] [t-012] B");

        // LF document, trailing newline kept.
        let lf = "- [ ] [t-013] A\n";
        let (updated, flipped) = check_off_content(lf, "t-013");
        assert!(flipped);
        assert_eq!(updated, "- [x] [t-013] A\n");

        // Mixed: each line keeps its own terminator.
        let mixed = "- [ ] [t-014] A\r\n- [ ] [t-015] B\n";
        let (updated, flipped) = check_off_content(mixed, "t-014");
        assert!(flipped);
        assert_eq!(updated, "- [x] [t-014] A\r\n- [ ] [t-015] B\n");
    }

    #[test]
    fn normalize_newlines_is_the_single_normalizer() {
        assert_eq!(normalize_newlines("a\r\nb\rc\n"), "a\nb\nc\n");
        // Already-normalized text is handed back unchanged.
        assert_eq!(normalize_newlines("a\nb\n"), "a\nb\n");
        // No trailing newline is invented.
        assert_eq!(normalize_newlines("a\r\nb"), "a\nb");
        assert_eq!(normalize_newlines(""), "");
        // CR inside a word (rare, but must not be treated as a line break when
        // it is part of a CRLF pair only once).
        assert_eq!(normalize_newlines("a\r\r\nb"), "a\n\nb");
    }

    #[test]
    fn description_starts_after_the_id_decoration() {
        for (line, expected) in [
            ("- [ ] [t-001] Build the parser", "Build the parser"),
            ("- [ ] (t-002) Migrate schema", "Migrate schema"),
            ("- [ ] **[t-003]** Docs refreshed", "Docs refreshed"),
            ("- [ ] `t-004` Backticked", "Backticked"),
            ("- [ ] t-005 bare form", "bare form"),
            ("- [ ] **t-006** Bold bare", "Bold bare"),
        ] {
            let task = parse_task_line(line).unwrap_or_else(|| panic!("task line: {line:?}"));
            assert_eq!(task.description, expected, "description for {line:?}");
            assert!(
                task.id_span
                    .is_some_and(|(s, e)| task.raw[s..e]
                        .eq_ignore_ascii_case(task.task_id.as_deref().unwrap())),
                "id span must cover the id token for {line:?}"
            );
        }
    }

    #[test]
    fn task_id_token_recogniser() {
        assert_eq!(
            find_task_id_token("MISSION COMPLETE (t-014)").as_deref(),
            Some("t-014")
        );
        assert_eq!(
            find_task_id_token("[t-100a1] subtask").as_deref(),
            Some("t-100a1")
        );
        assert_eq!(find_task_id_token("no id here").as_deref(), None);
        assert_eq!(find_task_id_token("xt-001 glued").as_deref(), None);
        assert!(is_task_id_token("t-001"));
        assert!(is_task_id_token("T-100a1"));
        assert!(!is_task_id_token("t-"));
        assert!(!is_task_id_token("task-1"));
    }

    #[test]
    fn task_id_eq_uses_the_canonical_normalizer() {
        for decorated in ["[t-001]", "(t-001)", "\"t-001\"", " t-001 ", "t-001"] {
            assert!(
                task_id_eq(decorated, "t-001"),
                "{decorated} must equal t-001"
            );
            assert!(
                task_id_eq("t-001", decorated),
                "t-001 must equal {decorated}"
            );
        }
        assert!(task_id_eq("T-001", "t-001"));
        assert!(!task_id_eq("t-100", "t-100a1"));
        assert!(!task_id_eq("t-100", "[]"));
        assert!(!task_id_eq("", "t-100"));
    }

    #[test]
    fn line_matches_task_id_owns_its_id_strictly() {
        let line = "- [ ] [t-100a1] Subtask A1 (depends on t-100)";
        assert_eq!(task_id_of_line(line).as_deref(), Some("t-100a1"));
        assert!(line_matches_task_id(line, "t-100a1"));
        assert!(!line_matches_task_id(line, "t-100"));
        assert!(!line_matches_task_id(line, "t-1"));
        assert!(!line_matches_task_id(line, "[]"));
        assert!(line_matches_task_id(
            "Prose that mentions t-100 in passing",
            "t-100"
        ));
        assert_eq!(task_id_of_line("# Plan header"), None);
        assert_eq!(task_id_of_line("- [ ] No task id here"), None);
    }

    #[test]
    fn role_hint_and_description_split() {
        let task = parse_task_line("- [ ] [t-001] Investigate API contracts (researcher)")
            .expect("task line");
        assert_eq!(task.description, "Investigate API contracts (researcher)");
        assert_eq!(
            task.description_without_role_hint(),
            "Investigate API contracts"
        );
        assert_eq!(task.role_hint.as_deref(), Some("researcher"));

        let bare = parse_task_line("- [ ] [t-002] Migrate schema").expect("task line");
        assert_eq!(bare.description, "Migrate schema");
        assert_eq!(bare.description_body, "Migrate schema");
        assert_eq!(bare.role_hint, None);

        let id_in_text =
            parse_task_line("- [ ] [t-011] Report (t-006...t-008)").expect("task line");
        assert_eq!(id_in_text.task_id.as_deref(), Some("t-011"));
        assert_eq!(id_in_text.description_without_role_hint(), "Report");
    }

    #[test]
    fn strip_list_marker_matches_summary_display_needs() {
        assert_eq!(strip_list_marker("- [ ] [t-001] Task"), "[ ] [t-001] Task");
        assert_eq!(strip_list_marker("1. [x] (t-002) Task"), "[x] (t-002) Task");
        assert_eq!(strip_list_marker("  * [ ] t-003 Task"), "[ ] t-003 Task");
    }

    // ---------------------------------------------------------------------
    // Recon L9 — fenced code blocks are documentation, not tasks.
    // ---------------------------------------------------------------------

    /// A plan that documents its own checklist format inside a fenced block.
    /// The fenced lines look exactly like real task lines (both spellings of
    /// `t-phantom*`) and must be invisible to every document-level scan.
    const FENCED_FIXTURE: &str = "\
# Execution Plan

The plan format is:

```text
- [ ] [t-phantom] Example line
- [x] [t-phantom-done] Completed example
```

- [x] [t-100] Real task, already done
- [ ] [t-101] Real task, still open
";

    /// Witness for the pre-L9 scan: every line of the document, fences ignored.
    fn legacy_unfenced_pending_task_ids(text: &str) -> Vec<String> {
        text.lines()
            .enumerate()
            .filter_map(|(idx, line)| parse_task_line_at(line, idx))
            .filter(|task| task.is_pending())
            .filter_map(|task| task.task_id)
            .collect()
    }

    #[test]
    fn fenced_examples_are_never_tasks() {
        // RED witness: the pre-L9 document scan ignored fences, so the plan
        // advertised a phantom pending task (delegatable, counted in progress).
        assert_eq!(
            legacy_unfenced_pending_task_ids(FENCED_FIXTURE),
            vec!["t-phantom".to_string(), "t-101".to_string()],
            "witness must reproduce the phantom-task bug"
        );

        let tasks = parse_tasks(FENCED_FIXTURE);
        assert_eq!(
            all_task_ids(FENCED_FIXTURE),
            vec!["t-100".to_string(), "t-101".to_string()],
            "fenced ids must not appear as tasks: {tasks:?}"
        );
        assert_eq!(
            unchecked_task_ids(FENCED_FIXTURE),
            vec!["t-101".to_string()]
        );
        assert_eq!(pending_lines(FENCED_FIXTURE).len(), 1);
        assert_eq!(completed_lines(FENCED_FIXTURE).len(), 1);
        assert!(
            find_task_line(FENCED_FIXTURE, "t-phantom").is_none(),
            "the fenced example must not be a findable task"
        );
        assert!(!is_pending_task(FENCED_FIXTURE, "t-phantom"));

        // Absolute line indices are preserved (the UI highlights that exact line).
        let expected = FENCED_FIXTURE
            .lines()
            .position(|l| l.contains("[t-101]"))
            .expect("t-101 line");
        assert_eq!(first_unchecked_line_index(FENCED_FIXTURE), Some(expected));
        assert_eq!(
            scannable_lines(FENCED_FIXTURE)
                .map(|(idx, _)| idx)
                .collect::<Vec<_>>(),
            vec![0, 1, 2, 3, 8, 9, 10],
            "fence delimiters and fenced bodies must both be skipped"
        );
    }

    #[test]
    fn fenced_checkbox_examples_cannot_hold_a_plan_open() {
        // Real work still open -> the gate stays open because of the REAL line.
        assert!(has_unchecked_box(FENCED_FIXTURE));

        // Same document with the real tasks ticked: the fenced `- [ ]` example
        // must not keep the plan incomplete.
        let done = FENCED_FIXTURE.replace("- [ ] [t-101]", "- [x] [t-101]");
        // RED witness: the old whole-document scan reported an unchecked box,
        // so `Plan::is_complete` stayed false and the plan never archived.
        assert!(
            UNCHECKED_BOX_RE.is_match(&done),
            "witness must reproduce the never-complete bug"
        );
        assert!(
            !has_unchecked_box(&done),
            "a fenced checklist example must not keep the plan incomplete:\n{done}"
        );
        assert!(has_checked_box(&done));

        // A document that is *only* a fenced example has no boxes at all.
        let only_fence = "Docs:\n\n```\n- [ ] [t-ghost] example\n```\n";
        assert!(!has_unchecked_box(only_fence));
        assert!(!has_checked_box(only_fence));
        assert!(unchecked_task_ids(only_fence).is_empty());
    }

    #[test]
    fn check_off_never_ticks_a_fenced_example() {
        // The phantom id is not checkable at all.
        let (untouched, flipped) = check_off_content(FENCED_FIXTURE, "t-phantom");
        assert!(!flipped, "a fenced example id must not be checkable");
        assert_eq!(untouched, FENCED_FIXTURE);

        // The real id ticks the real line, and the fenced example keeps its
        // unchecked box verbatim.
        let (updated, flipped) = check_off_content(FENCED_FIXTURE, "t-101");
        assert!(flipped);
        assert!(updated.contains("- [ ] [t-phantom] Example line"));
        assert!(updated.contains("- [x] [t-phantom-done] Completed example"));
        assert!(updated.contains("- [x] [t-101] Real task, still open"));
        assert_eq!(unchecked_task_ids(&updated), Vec::<String>::new());
    }

    #[test]
    fn fence_rules_tildes_lengths_and_inline_backticks() {
        // Tilde fence, and a longer fence that survives a shorter inner run.
        let doc = "\
~~~~markdown
- [ ] [t-in-fence] example
``` nested shorter run stays inside
- [ ] [t-still-in-fence] example
~~~~
- [ ] [t-real] real task
";
        assert_eq!(all_task_ids(doc), vec!["t-real".to_string()]);

        // Inline backticks mid-line are not fence delimiters.
        let inline = "- [ ] [t-102] Document ``` usage in the plan\n";
        assert_eq!(all_task_ids(inline), vec!["t-102".to_string()]);
        assert!(has_unchecked_box(inline));

        // An unterminated fence suppresses the rest of the document (what
        // Markdown itself does) — never resurrect half a document.
        let unclosed = "- [ ] [t-103] before fence\n```\n- [ ] [t-104] after fence\n";
        assert_eq!(all_task_ids(unclosed), vec!["t-103".to_string()]);
        assert_eq!(first_unchecked_line_index(unclosed), Some(0));
    }

    #[test]
    fn completion_gates_keep_historical_semantics() {
        assert!(has_unchecked_box("- [ ] [t-001] a"));
        assert!(has_unchecked_box("- ( ) (t-001) a"));
        assert!(!has_unchecked_box("- [x] [t-001] a"));
        assert!(has_checked_box("- [x] [t-001] a"));
        assert!(has_checked_box("- (X) (t-001) a"));
        assert!(!has_checked_box("- [ ] [t-001] a"));
    }

    // ---------------------------------------------------------------------
    // Cross-site agreement + on-disk regression guards.
    // ---------------------------------------------------------------------

    /// One fixture plan exercising the drifted forms, shared by the agreement
    /// tests below.
    const FIXTURE: &str = "\
# Execution Plan

### Phase 1
- [x] [t-c3agg-1] Baseline captured
- [ ] (t-c3agg-2) Migrate schema
* [ ] t-c3agg-3 Add regression tests
- [X] **[t-c3agg-4]** Docs refreshed
- [ ] [t-c3agg-5] Ship it
";

    #[test]
    fn regression_parenthesised_task_line_is_pending_and_checkable_on_disk() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plan = crate::manager::phase::Plan::at(dir.path());
        plan.create("# Execution Plan\n- [ ] (t-002) migrate schema\n")
            .expect("create plan");

        // Was invisible before the shared grammar (`docs/recon_bugs_harness_llm_ui.md` §3 C1).
        assert_eq!(plan.pending_tasks(), vec!["t-002".to_string()]);
        assert_eq!(plan.all_tasks(), vec!["t-002".to_string()]);
        assert!(!plan.is_complete());

        assert!(plan.check_off("t-002").expect("check_off"), "must flip");
        let disk = plan.read().expect("read").expect("present");
        assert!(
            disk.contains("- [x] (t-002) migrate schema"),
            "original id spelling must be preserved:\n{disk}"
        );
        assert!(plan.pending_tasks().is_empty());
        assert!(plan.is_complete());
        // Id may also be addressed decorated.
        assert!(!plan.check_off("[t-002]").expect("already checked"));
    }

    #[test]
    fn cross_site_agreement_plan_parse_vs_phase_summaries_and_ui() {
        let fixture = FIXTURE;

        // 1. plan_parse's own view.
        let all = all_task_ids(fixture);
        let pending = unchecked_task_ids(fixture);
        assert_eq!(
            all,
            vec![
                "t-c3agg-1".to_string(),
                "t-c3agg-2".to_string(),
                "t-c3agg-3".to_string(),
                "t-c3agg-4".to_string(),
                "t-c3agg-5".to_string(),
            ]
        );
        assert_eq!(
            pending,
            vec![
                "t-c3agg-2".to_string(),
                "t-c3agg-3".to_string(),
                "t-c3agg-5".to_string(),
            ]
        );

        // 2. The on-disk authority (`manager::phase`) must agree byte-for-byte.
        let dir = tempfile::tempdir().expect("temp dir");
        let plan = crate::manager::phase::Plan::at(dir.path());
        plan.create(fixture).expect("create fixture");
        assert_eq!(plan.all_tasks(), all, "Plan::all_tasks diverged");
        assert_eq!(
            plan.pending_tasks(),
            pending,
            "Plan::pending_tasks diverged"
        );

        // 3. `orchestrator::plan_summary` counts must match the same fixture.
        let summary = crate::orchestrator::generate_plan_progress_summary(fixture);
        assert!(
            summary.contains("Overall Progress: 2/5 tasks completed (40%)"),
            "summary counts diverge from the plan grammar:\n{summary}"
        );
        assert!(
            summary.contains(&format!("### Pending Steps ({}/5):", pending.len())),
            "pending count diverges:\n{summary}"
        );
        let completed_section = summary
            .split("### Completed Steps")
            .nth(1)
            .unwrap_or("")
            .split("### ")
            .next()
            .unwrap_or("");
        let pending_section = summary.split("### Pending Steps").nth(1).unwrap_or("");
        for id in &all {
            let section = if pending.contains(id) {
                pending_section
            } else {
                completed_section
            };
            assert!(
                section.contains(id.as_str()),
                "task {id} missing from the summary section it belongs to:\n{summary}"
            );
        }

        // 4. `ui::tui::formatting` must report the same ids/offsets.
        let ui_ids: Vec<Option<String>> = fixture
            .lines()
            .map(crate::ui::tui::formatting::extract_plan_line_task_id)
            .collect();
        assert_eq!(
            ui_ids,
            vec![
                None,
                None,
                None,
                Some("t-c3agg-1".to_string()),
                Some("t-c3agg-2".to_string()),
                Some("t-c3agg-3".to_string()),
                Some("t-c3agg-4".to_string()),
                Some("t-c3agg-5".to_string()),
            ]
        );
        assert_eq!(
            crate::ui::tui::formatting::visual_line_offset_of_first_pending(fixture, 500),
            first_unchecked_line_index(fixture),
            "UI first-pending offset diverges from the shared grammar"
        );
        assert_eq!(
            crate::ui::tui::formatting::visual_line_offset_of_task(fixture, "t-c3agg-2", 500),
            first_unchecked_line_index(fixture),
            "UI task offset diverges for the parenthesised id"
        );
        for id in &pending {
            assert!(
                fixture
                    .lines()
                    .any(|l| crate::ui::tui::formatting::line_matches_task_id(l, id)),
                "UI cannot locate pending task {id}"
            );
        }
    }
}
