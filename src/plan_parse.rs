//! Single owner of the execution-plan **task-line grammar**: one task-line
//! parser (checkbox state + task id + byte/line offsets), one `check_off` line
//! rewriter, and one task-id token recogniser. `manager::phase` stays the
//! authority for the on-disk mutation (it owns the files and the `PLAN_MUTEX`);
//! every other site asks this module what a task line is, so summaries, briefs,
//! prompts and the UI all agree with the disk.
//!
//! One grammar for every reader: list markers `- * + 1. 1)`, checkboxes `[ ]`,
//! `[]`, `[x]`, `[X]`, `( )`, `(x)`, `(X)`, bold/backtick decoration, and id
//! spellings `[t-001]`, `(t-001)` or bare `t-001` — a plan written as
//! `- [ ] (t-002) migrate schema` is a pending task. The id bound to a checkbox
//! sits at a word boundary: \[?<tid>\]? after optional bold/italic decoration.
//!
//! A line may additionally carry an explicit validation directive from a closed,
//! reserved set, written **before** the role hint — `- [ ] [t-001] Investigate
//! library X (no-validate) (researcher)`. It parses into
//! [`ValidationDirective`] and is stripped from `description_body` (never from
//! `raw`), so the token stops leaking into briefs, summaries and prompts while
//! the on-disk line stays byte-for-byte intact.
//!
//! Check-off is what keeps that promise *after* a rewrite: [`TaskLine::checked_line`]
//! splices `checkbox_span` and nothing else, so a directive-bearing line survives
//! `- [ ]` → `- [x]` byte-for-byte apart from the box itself — the reserved token
//! is never dropped, duplicated or re-ordered, and `checkbox_span`/`id_span` stay
//! valid. The placement rule follows as a corollary: a reserved token written
//! **before** the id (`- [ ] (no-validate) [t-001] X`) is never mistaken for an id
//! (it is not `t-`-shaped), and the line degrades to an id-less checklist item that
//! no task id can check off.
//!
//! The role hint is a closed set too: a trailing paren group is a role hint only
//! when its content names a delegatable archetype exactly (case-insensitively,
//! as accepted by `agents::Agent::from_str`). Every other trailing group —
//! `(see notes)`, `(draft (v2))`, `(t-006...t-008)`, `( )`, an unclosed tail —
//! stays inside `description_body` with `role_hint == None`, so a prose
//! parenthetical can never be routed to a specialist through a bogus hint.
//!
//! Deliberately out of scope (owned elsewhere, unchanged): the *marker* grammar
//! and the verdict that is allowed to flip a box — [`crate::markers::MissionMarker`],
//! applied to the plan by `manager::phase::Plan::check_plan_on_deliverable` —
//! and task-id **normalization** (`crate::task_id`, re-used here rather than
//! re-implemented). A box is flipped only from an explicitly resolved task id,
//! never from output prose.

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

/// The closed, reserved validation vocabulary a plan task line may carry as an
/// explicit directive: `(validate)` / `(no-validate)`.
///
/// The grammar places the directive **before** the role hint —
/// `- [ ] [t-001] Investigate library X (no-validate) (researcher)` — and the
/// token set is closed and matched exactly (case-insensitively), see
/// [`VALIDATION_DIRECTIVES`]. Anything outside the set — a prose parenthetical
/// such as `(see notes)`, an unknown token such as `(maybe-validate)`, or no
/// token at all — is [`ValidationDirective::Auto`]: lookup is **fail-closed**,
/// so a typo can never silently turn validation off.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ValidationDirective {
    /// No reserved directive token on the line: downstream policy decides.
    #[default]
    Auto,
    /// `(validate)`.
    Validate,
    /// `(no-validate)`.
    NoValidate,
}

/// The closed reserved directive vocabulary, in the lower-cased spelling the
/// matcher compares against. This is the only place the words are spelled.
pub const VALIDATION_DIRECTIVES: &[&str] = &["validate", "no-validate"];

impl ValidationDirective {
    /// Map one token onto the closed set, case-insensitively and ignoring
    /// surrounding whitespace (`(NO-VALIDATE )` → [`Self::NoValidate`]).
    /// Every token outside [`VALIDATION_DIRECTIVES`] yields [`Self::Auto`] —
    /// never [`Self::NoValidate`].
    #[must_use]
    pub fn parse(token: &str) -> Self {
        let token = token.trim().to_ascii_lowercase();
        match token.as_str() {
            "validate" => Self::Validate,
            "no-validate" => Self::NoValidate,
            _ => Self::Auto,
        }
    }

    /// `true` when the line carries no reserved directive token.
    #[must_use]
    pub const fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }
}

impl std::str::FromStr for ValidationDirective {
    type Err = std::convert::Infallible;

    /// [`Self::parse`] behind the standard trait: a token outside the closed set
    /// is `Ok(Auto)`, never an error, so no caller can be tempted into
    /// inventing its own default.
    fn from_str(token: &str) -> Result<Self, Self::Err> {
        Ok(Self::parse(token))
    }
}

/// One parsed plan task line.
///
/// Spans are byte offsets **within `raw`**; `line_number` is the 0-based line
/// index within the document the line came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskLine {
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
    /// trailing reserved `(validate)` / `(no-validate)` group, a trailing
    /// `(role)` hint and any prose parenthetical. **Not** the field briefs use:
    /// [`TaskLine::description_body`] is authoritative for delegation briefs and
    /// every other model-facing text, because it carries neither the reserved
    /// directive nor the routing hint. `description` is kept verbatim for
    /// span/round-trip purposes — it is the text `description_body` was derived
    /// from, so the pair records exactly what was stripped.
    pub description: String,
    /// [`TaskLine::description`] with the trailing reserved directive group and a
    /// trailing **archetype** role hint removed. A prose parenthetical
    /// (`(see notes)`, `(draft (v2))`) stays where it was written.
    pub description_body: String,
    /// Lower-cased trailing role-hint token of the description, e.g.
    /// `(researcher)` → `Some("researcher")`, but **only** when the trailing
    /// paren group names a delegatable archetype ([`split_role_hint`], closed
    /// set). `None` when the line carries no hint — including a prose group such
    /// as `(see notes)`, which is never turned into a bogus hint.
    pub role_hint: Option<String>,
    /// Reserved validation directive of the line, parsed from a trailing
    /// `(validate)` / `(no-validate)` group that sits **before** the role hint.
    /// [`ValidationDirective::Auto`] when the line carries no reserved token —
    /// the token never leaks into [`TaskLine::description_body`], and `raw`
    /// stays byte-for-byte what the plan says.
    pub validation_directive: ValidationDirective,
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
    /// (notably the original task-id spelling such as `(t-002)`, and a trailing
    /// `(no-validate)` / `(researcher)` group: only [`Self::checkbox_span`] is
    /// spliced, the rest of `raw` is copied verbatim, so a reserved token can
    /// never be dropped, duplicated or re-ordered by a check-off).
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

/// Bare task-id token (no boundary context), for validating an adjacent token.
static TASK_ID_STRICT_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)^t-[A-Za-z0-9_-]+$").expect("valid strict task id regex"));

/// Whole-document unchecked-box scan (`\[\s*\]|\(\s*\)`). Completion is a
/// whole-document text property, not a task-line grammar concern.
static UNCHECKED_BOX_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"\[\s*\]|\(\s*\)").expect("valid unchecked box regex"));

/// Whole-document checked-box scan (`\[[xX]\]|\([xX]\)`).
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

    // Directive scan runs **before** role-hint extraction so the reserved token
    // is never swallowed as a role, and never leaks into `description_body`.
    let (description_body, validation_directive) = split_validation_directive(description);
    let (description_body, role_hint) = split_role_hint(&description_body);

    Some(TaskLine {
        checkbox,
        task_id,
        checkbox_span: (box_start, box_end),
        id_span,
        description: description.to_string(),
        description_body,
        role_hint,
        validation_directive,
        line_number,
        raw: line.to_string(),
    })
}

/// Parse every task line (checked **and** unchecked) of a plan document.
///
/// Lines sitting **inside a fenced code block** are skipped: a plan that
/// documents its own `- [ ] [t-xxx]` format inside a ```` ``` ```` block must
/// not report phantom pending task ids to `pending_tasks()`, the progress
/// summary or a delegation brief. Line grammar unchanged (see [`parse_task_line`]).
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
/// [`parse_tasks`].
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
/// `task_id`. Returns new content plus whether anything flipped — the body of
/// `manager::phase::Plan::check_off`. **Byte-faithful**: each line keeps its own
/// terminator and the document keeps (or keeps out) its trailing newline,
/// newline normalization having exactly one owner ([`normalize_newlines`]).
/// Lines inside fenced code blocks are not candidates, so a quoted
/// `- [ ] [t-001] …` example is never ticked instead of the real task line; the
/// candidate set is exactly what [`parse_tasks`] and [`find_task_line`] read.
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
/// This is the crate's **only** newline normalizer for plan text, applied exactly
/// once — when the plan is written by `manager::phase::Plan::create` — so every
/// later read/scan/check-off sees one spelling of end-of-line and nothing else
/// (not `check_off`, not the UI) re-writes line endings as a side effect. A
/// trailing newline is *not* added here: that is the writer's decision, and
/// `check_off_content` preserves whatever was chosen.
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
/// completion check).
///
/// Scanned line by line so a `- [ ]` template sitting **inside a fenced code
/// block** cannot hold a plan open forever: a fence-only match would keep
/// `has_unchecked_box == true` while every real task is ticked, so
/// `Plan::is_complete` would never fire and the plan would never archive. `\s`
/// inside a checkbox never matches a line break — no writer produces that.
#[must_use]
pub fn has_unchecked_box(text: &str) -> bool {
    scannable_lines(text).any(|(_, line)| UNCHECKED_BOX_RE.is_match(line))
}

/// `true` when the text contains a checked checkbox anywhere (plan completion
/// check). Fence-aware for the same reason as [`has_unchecked_box`].
#[must_use]
pub fn has_checked_box(text: &str) -> bool {
    scannable_lines(text).any(|(_, line)| CHECKED_BOX_RE.is_match(line))
}

/// Strip a leading list marker / blockquote / table gutter from a plan line for
/// display (shared with `orchestrator::plan_summary`).
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

/// The closed vocabulary of role hints: `true` when `token` names a delegatable
/// agent archetype.
///
/// There is deliberately **no second list** here — the vocabulary is exactly what
/// [`crate::agents::Agent::from_str`] accepts (`src/agents/mod.rs`, the six
/// built-in archetypes plus the `deepbrain` alias; custom `AGENTS.md` archetypes
/// are not delegatable, see `orchestrator::registry::SpecialistRegistry::canonical`).
/// That is the same resolver the delegation layer applies downstream
/// (`agents::prompt_builder`), so grammar and delegation cannot drift: anything
/// the resolver rejects is prose and can never be handed to it as a hint.
/// [`crate::agents::Agent::from_str`] already trims and lower-cases, so the match
/// is case- and whitespace-insensitive by construction.
#[must_use]
fn is_role_archetype(token: &str) -> bool {
    crate::agents::Agent::from_str(token).is_some()
}

/// Split a trailing `(role)` hint off a task description (`(researcher)`) —
/// **closed set**: the group is a hint only if it names an archetype exactly
/// (case-insensitive, trimmed), see [`is_role_archetype`].
///
/// Lookup is fail-closed, mirroring [`ValidationDirective::parse`]. Anything else
/// is prose: it stays verbatim inside the returned body and the hint is `None`,
/// so an unrecognised token can no longer reach the
/// `Agent::from_str(hint).unwrap_or(Agent::Coder)` fallback of
/// `agents::prompt_builder` and silently re-route a task. The default-agent
/// behaviour of a line that genuinely carries no hint is untouched: `None` is
/// what those lines already produced.
///
/// The group examined is the **balanced** paren group that ends the text
/// ([`matching_open_paren`], nesting-aware), which fixes the recorded bugs:
/// - `(see notes)` / `(t-006...t-008)` / `(researcherx)` → prose (was
///   `Some("see notes")` / `Some("t-006...t-008")` / `Some("researcherx")`);
/// - `(draft (v2))` → one group whose content is `draft (v2` → prose (was
///   `Some("v2)")`; an archetype token contains no parenthesis, so a nested
///   group can never match);
/// - `()` / `( )` → prose (was `Some("")`) and the line still parses as a task;
/// - an unclosed or unbalanced tail — `(researcher` or `researcher)` — has no
///   opener matching the trailing `)`, so it is **not** a role hint either;
/// - anything after the closing paren, e.g. `(researcher)*`, kills the match
///   exactly as before.
///
/// The hint token keeps its previous shape (parens stripped, trimmed,
/// lower-cased), and a prose group sitting left of a real hint does not block
/// it: `(see notes) (coder)` still yields `coder`. Only the **trailing** group has
/// to balance — a stray unmatched paren earlier in the prose (`Step 2) … (coder)`)
/// does not cost a task its role hint, because the role slot is that trailing
/// group and nothing else.
#[must_use]
pub fn split_role_hint(description: &str) -> (String, Option<String>) {
    let text = description.trim();
    if !text.ends_with(')') {
        return (text.to_string(), None);
    }
    let close = text.len() - 1;
    // `matching_open_paren` returns the `(` balancing that `)`, so
    // `open + 1 <= close` always holds and the slice below is in bounds — for
    // `()` it is simply the empty token, which is not an archetype.
    let Some(open) = matching_open_paren(text) else {
        return (text.to_string(), None);
    };
    let candidate = text[open + 1..close].trim();
    if !is_role_archetype(candidate) {
        return (text.to_string(), None);
    }
    (
        text[..open].trim().to_string(),
        Some(candidate.to_ascii_lowercase()),
    )
}

/// Split a reserved validation directive — `(validate)` / `(no-validate)` — off
/// a task description. Runs **before** [`split_role_hint`] so the token can
/// never be swallowed as a role hint (the collision with validation directives)
/// and can never leak into the body used
/// for delegation briefs, plan summaries and the auditor prompt.
///
/// Trailing parenthesised groups are examined bracket-group-by-bracket-group
/// from the right, nesting-aware — never "the last paren only" — so the
/// canonical spelling `Investigate X (no-validate) (researcher)` yields both the
/// directive and the role hint. Classification is an exact, case-insensitive
/// lookup in the closed set [`VALIDATION_DIRECTIVES`]; every other group
/// (`(see notes)`, `(maybe-validate)`, `(NO-VALIDATE)x`) stays prose.
///
/// The right-most trailing group is the slot [`split_role_hint`] claims for the
/// role hint, so it is a directive candidate only when it is the *only* trailing
/// group. That deliberately pins the documented reversed-order collision:
/// `X (researcher) (no-validate)` still reports [`ValidationDirective::Auto`],
/// unchanged — making the reversed spelling resolve the directive belongs to
/// t-013c/t-014a. What closed-set role extraction (t-013b) changed is only the
/// *collateral*: the trailing reserved group is no longer swallowed as a bogus
/// role hint either, so that line now reports `role_hint == None` and keeps both
/// groups in `description_body`.
#[must_use]
pub fn split_validation_directive(description: &str) -> (String, ValidationDirective) {
    let text = description.trim();
    // Trailing paren groups, right to left, as (open byte, exclusive close byte).
    // The scan stops at the first step left that is not a paren group; the
    // whitespace *between* groups is skipped, never treated as a break.
    let mut groups: Vec<(usize, usize)> = Vec::new();
    let mut end = text.len();
    while end > 0 && text[..end].ends_with(')') {
        let Some(open) = matching_open_paren(&text[..end]) else {
            break;
        };
        groups.push((open, end));
        end = text[..open].trim_end().len();
    }
    // Directive candidates: everything left of the right-most (role-hint) group,
    // or that group itself when it is the only trailing group on the line.
    let candidates: &[(usize, usize)] = match groups.len() {
        0 => &[],
        1 => &groups,
        _ => &groups[1..],
    };
    for &(open, close) in candidates {
        let directive = ValidationDirective::parse(&text[open + 1..close - 1]);
        if directive.is_auto() {
            continue;
        }
        let before = text[..open].trim_end();
        let after = text[close..].trim_start();
        let body = match (before.is_empty(), after.is_empty()) {
            (true, true) => String::new(),
            (true, false) => after.to_string(),
            (false, true) => before.to_string(),
            (false, false) => format!("{before} {after}"),
        };
        return (body, directive);
    }
    (text.to_string(), ValidationDirective::Auto)
}

/// Byte index of the `(` balancing the **trailing** `)` of `text`, nesting-aware:
/// for `(draft (v2)` it is the `(` of `(v2)`, while for the fully closed
/// `(draft (v2))` it is the **outer** one (so the content the closed-set matchers
/// see is `draft (v2`, which they reject). ASCII-only by design: the grammar uses
/// ASCII parens and a UTF-8 continuation byte can never collide with `(` or `)`.
/// `None` when the trailing `)` has no opener.
fn matching_open_paren(text: &str) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth = 0usize;
    for (index, byte) in bytes.iter().enumerate().rev() {
        match byte {
            b')' => depth += 1,
            b'(' if depth > 0 => {
                depth -= 1;
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
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
    use crate::agents::Agent;

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
        // The rewrite is byte-faithful: the trailing newline the document had
        // is still there, and only the one checkbox changed.
        assert_eq!(
            updated,
            "- [ ] [t-010] Tenth\n- [x] [t-01] First\n- [ ] [t-01] Again\n"
        );
        let (same, flipped) = check_off_content(content, "t-404");
        assert!(!flipped);
        assert_eq!(same, content);
    }

    /// Contrast rewrite for the same fixtures: joining lines with `"\n"` drops
    /// the final newline and flattens CRLF, so ticking one box re-writes the
    /// whole document.
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

        // Updated by t-013b: this used to pin the old "any trailing paren group is
        // a role hint" grammar (`Some("t-006...t-008")`, body `"Report"`). A
        // task-id range inside the prose is prose, so the body keeps it verbatim.
        let id_in_text =
            parse_task_line("- [ ] [t-011] Report (t-006...t-008)").expect("task line");
        assert_eq!(id_in_text.task_id.as_deref(), Some("t-011"));
        assert_eq!(id_in_text.role_hint, None);
        assert_eq!(
            id_in_text.description_without_role_hint(),
            "Report (t-006...t-008)"
        );
    }

    // Closed-set role hint (t-013b): a trailing paren group is a role hint only
    // when it names a delegatable archetype. Fixtures are
    // (line, role hint, description body) — `description` and `raw` stay verbatim.
    const ROLE_HINT_FIXTURES: &[(&str, Option<&str>, &str)] = &[
        // Recognised archetypes: exact spelling, upper case, whitespace-padded.
        (
            "- [ ] [t-030] Investigate API contracts (researcher)",
            Some("researcher"),
            "Investigate API contracts",
        ),
        (
            "- [ ] [t-031] Investigate API contracts (RESEARCHER)",
            Some("researcher"),
            "Investigate API contracts",
        ),
        (
            "- [ ] [t-032] Investigate API contracts ( Researcher )",
            Some("researcher"),
            "Investigate API contracts",
        ),
        // Every delegatable archetype resolves, aliases included: the grammar
        // accepts exactly what `Agent::from_str` accepts (see the drift guard).
        ("- [ ] [t-033] Ship it (coder)", Some("coder"), "Ship it"),
        (
            "- [ ] [t-034] Bisect it (Debugger)",
            Some("debugger"),
            "Bisect it",
        ),
        (
            "- [ ] [t-035] Audit it (validator)",
            Some("validator"),
            "Audit it",
        ),
        (
            "- [ ] [t-036] Cover it (generalist)",
            Some("generalist"),
            "Cover it",
        ),
        (
            "- [ ] [t-037] Sequence it (planner)",
            Some("planner"),
            "Sequence it",
        ),
        (
            "- [ ] [t-038] Reason it through (deepbrain)",
            Some("deepbrain"),
            "Reason it through",
        ),
        // Prose parentheticals: no hint, text preserved in `description_body`.
        (
            "- [ ] [t-039] Write the report (see notes)",
            None,
            "Write the report (see notes)",
        ),
        (
            "- [ ] [t-040] Fix it (researcherx)",
            None,
            "Fix it (researcherx)",
        ),
        (
            "- [ ] [t-041] Fix it (xresearcher)",
            None,
            "Fix it (xresearcher)",
        ),
        ("- [ ] [t-042] Fix the coder", None, "Fix the coder"),
        (
            "- [ ] [t-043] Report (t-006...t-008)",
            None,
            "Report (t-006...t-008)",
        ),
        // Nested group: the group ending the line is `(draft (v2))`, whose
        // content can never be an archetype — no `"v2)"` fragment any more.
        ("- [ ] [t-044] Note (draft (v2))", None, "Note (draft (v2))"),
        // Empty groups are prose (they used to yield `Some("")`).
        ("- [ ] [t-045] Fix it ( )", None, "Fix it ( )"),
        ("- [ ] [t-046] Fix it ()", None, "Fix it ()"),
        // Unclosed / unbalanced tails are not role hints either (documented rule).
        (
            "- [ ] [t-047] Fix it (researcher",
            None,
            "Fix it (researcher",
        ),
        (
            "- [ ] [t-048] Fix it researcher)",
            None,
            "Fix it researcher)",
        ),
        (
            "- [ ] [t-049] Fix it (researcher))",
            None,
            "Fix it (researcher))",
        ),
        // Emphasis or punctuation after the group kills the match, exactly as before.
        (
            "- [ ] [t-050] Fix it (researcher)*",
            None,
            "Fix it (researcher)*",
        ),
        (
            "- [ ] [t-051] Fix it (researcher).",
            None,
            "Fix it (researcher).",
        ),
        // Only the trailing group must be balanced: a stray paren earlier in the
        // prose does not cost the task its role hint (no regression for
        // `Step 2) … (coder)` style lines).
        (
            "- [ ] [t-052] Step 2) wire it up (coder)",
            Some("coder"),
            "Step 2) wire it up",
        ),
        // A prose group left of a real archetype does not block the hint.
        (
            "- [ ] [t-053] Write the report (see notes) (researcher)",
            Some("researcher"),
            "Write the report (see notes)",
        ),
        // A reserved directive token in the role slot is no archetype: the
        // directive scan claims that slot, and no bogus hint is produced.
        ("- [ ] [t-054] Fix it (no-validate)", None, "Fix it"),
        ("- [ ] [t-055] Fix it (validate)", None, "Fix it"),
    ];

    #[test]
    fn role_hint_is_a_closed_set_of_archetypes() {
        for (line, role_hint, body) in ROLE_HINT_FIXTURES {
            let task =
                parse_task_line(line).unwrap_or_else(|| panic!("must be a task line: {line:?}"));
            assert_eq!(
                task.role_hint.as_deref(),
                *role_hint,
                "role hint for {line:?}"
            );
            assert_eq!(task.description_body, *body, "body for {line:?}");
            assert_eq!(
                task.description_without_role_hint(),
                *body,
                "body accessor for {line:?}"
            );
            assert_eq!(
                task.description,
                line.rsplit_once("] ")
                    .expect("fixture lines carry a bracketed task id")
                    .1,
                "description must stay verbatim for {line:?}"
            );
            assert_eq!(task.raw, *line, "raw must be byte-identical: {line:?}");
        }
    }

    /// `( )` / `()` are prose, not empty role hints — and the line is still a
    /// task: id binding, checkbox and check-off spans are untouched.
    #[test]
    fn empty_parenthesised_groups_still_parse_as_tasks() {
        for (line, id, body) in [
            (
                "- [ ] [t-060] Fix the parser ( )",
                "t-060",
                "Fix the parser ( )",
            ),
            (
                "- [ ] [t-061] Fix the parser ()",
                "t-061",
                "Fix the parser ()",
            ),
        ] {
            let task = parse_task_line(line).unwrap_or_else(|| panic!("task line: {line:?}"));
            assert_eq!(task.task_id.as_deref(), Some(id));
            assert!(task.is_pending(), "{line:?} is a pending task");
            assert_eq!(task.role_hint, None, "{line:?} carries no role hint");
            assert_eq!(task.validation_directive, ValidationDirective::Auto);
            assert_eq!(task.description_body, body, "body for {line:?}");
            assert_eq!(task.description, body, "description for {line:?}");
            assert_eq!(task.raw, line);

            let flipped = check_off_line(line, id).expect("must flip");
            assert_eq!(
                flipped,
                format!("- [x] [{id}] {body}"),
                "check-off keeps every byte"
            );
            assert_eq!(
                parse_task_line(&flipped)
                    .expect("flipped line still parses")
                    .role_hint,
                None
            );
        }
        assert_eq!(
            unchecked_task_ids("- [ ] [t-062] Fix it ( )"),
            vec!["t-062".to_string()],
            "an empty group must not break the pending scan"
        );
    }

    /// Directive and role hint are independent slots, in either arrangement and
    /// with prose in between: both fields still come back.
    #[test]
    fn directive_and_role_hint_coexist_in_either_arrangement() {
        let canonical =
            parse_task_line("- [ ] [t-063] Investigate library X (no-validate) (researcher)")
                .expect("task line");
        assert_eq!(
            canonical.validation_directive,
            ValidationDirective::NoValidate
        );
        assert_eq!(canonical.role_hint.as_deref(), Some("researcher"));
        assert_eq!(canonical.description_body, "Investigate library X");

        let prose_between = parse_task_line("- [ ] [t-064] Fix it (validate) (see notes) (coder)")
            .expect("task line");
        assert_eq!(
            prose_between.validation_directive,
            ValidationDirective::Validate
        );
        assert_eq!(prose_between.role_hint.as_deref(), Some("coder"));
        assert_eq!(prose_between.description_body, "Fix it (see notes)");

        let reversed =
            parse_task_line("- [ ] [t-065] Fix it (coder) (no-validate)").expect("task line");
        // Reversed order is still not a directive (pinned, t-013c/t-014a) and the
        // archetype to its left stays prose — it is not the trailing group.
        assert_eq!(reversed.validation_directive, ValidationDirective::Auto);
        assert_eq!(reversed.role_hint, None);
        assert_eq!(reversed.description_body, "Fix it (coder) (no-validate)");
    }

    /// Drift guard: the role-hint vocabulary is **not** a second list — it is
    /// `agents::Agent::from_str`, cross-checked here against the delegatable
    /// registry so the grammar and the delegation layer cannot drift silently.
    #[test]
    fn role_hint_closed_set_matches_agent_from_str() {
        use crate::orchestrator::registry::SpecialistRegistry;

        let delegatable = SpecialistRegistry::canonical().agent_ids();
        assert!(!delegatable.is_empty(), "registry must expose the roles");
        for id in &delegatable {
            assert!(
                Agent::from_str(id).is_some(),
                "{id} is registered but unresolvable"
            );
            for spelling in [&id.to_string(), &id.to_uppercase(), &format!(" {id} ")] {
                let (body, hint) = split_role_hint(&format!("Migrate the schema ({spelling})"));
                assert_eq!(
                    hint.as_deref(),
                    Some(id.as_str()),
                    "'{spelling}' must be a role hint"
                );
                assert_eq!(body, "Migrate the schema", "'{spelling}' must be split off");
            }
        }

        // Grammar and resolver must agree on every token, in both directions.
        for token in [
            "",
            " ",
            "deepbrain",
            "coder",
            "CODER",
            "see notes",
            "researcherx",
            "xresearcher",
            "coders",
            "no-validate",
            "validate",
            "v2)",
            "draft (v2",
            "t-006...t-008",
            "the coder",
            "coder specialist",
            "coder,validator",
            "agent",
        ] {
            assert_eq!(
                is_role_archetype(token),
                Agent::from_str(token).is_some(),
                "grammar and `Agent::from_str` disagree about '{token}'"
            );
            let (body, hint) = split_role_hint(&format!("Migrate the schema ({token})"));
            if Agent::from_str(token).is_some() {
                assert_eq!(
                    hint.as_deref(),
                    Some(token.trim().to_ascii_lowercase().as_str()),
                    "'{token}' is an archetype and must be the (lower-cased) hint"
                );
                assert_eq!(body, "Migrate the schema");
            } else {
                assert_eq!(hint, None, "'{token}' must stay prose, never a hint");
                assert_eq!(body, format!("Migrate the schema ({token})"));
            }
        }
    }

    /// Requirement-level guard on the downstream bug: a prose parenthetical must
    /// no longer arrive at the `unwrap_or(Agent::Coder)` fallback as a bogus
    /// hint, while a line with genuinely no hint keeps its default agent and a
    /// real archetype still routes.
    #[test]
    fn prose_parentheticals_never_become_a_bogus_role_hint_for_the_coder_fallback() {
        use crate::agents::prompt_builder::{PlanTaskItem, PromptBuilder};

        let markdown = "\
# Execution Plan
- [ ] [t-070] Write the report (see notes)
- [ ] [t-071] Write the report
- [ ] [t-072] Write the report (researcher)
";
        let items = PromptBuilder::parse_tasks_from_plan(markdown);
        assert_eq!(items.len(), 3, "all three lines are pending tasks");

        let resolve = |item: &PlanTaskItem| {
            item.role_hint
                .as_deref()
                .and_then(Agent::from_str)
                .unwrap_or(Agent::Coder)
        };
        // Prose stays in the brief body and yields **no** hint at all.
        assert_eq!(items[0].description, "Write the report (see notes)");
        assert_eq!(items[0].role_hint, None);
        // Identical to the paren-less line: default agent unchanged.
        assert_eq!(
            resolve(&items[0]),
            resolve(&items[1]),
            "the default agent for a hint-less line must not move"
        );
        // A real archetype still routes.
        assert_eq!(resolve(&items[2]), Agent::Researcher);
        assert_eq!(items[2].description, "Write the report");
    }

    // Reserved validation directive: `(validate)` / `(no-validate)`, placed
    // before the role hint. Fixtures are (line, directive, role hint,
    // description body) — `description` and `raw` stay verbatim.
    const DIRECTIVE_FIXTURES: &[(&str, ValidationDirective, Option<&str>, &str)] = &[
        // Locked grammar: directive before the role hint.
        (
            "- [ ] [t-001] Investigate library X (no-validate) (researcher)",
            ValidationDirective::NoValidate,
            Some("researcher"),
            "Investigate library X",
        ),
        (
            "* [ ] (t-002) Ship the parser (validate) (coder)",
            ValidationDirective::Validate,
            Some("coder"),
            "Ship the parser",
        ),
        // Case-insensitive, whitespace-tolerant token, decorated id.
        (
            "- [ ] **[t-003]** Fix the bug (NO-VALIDATE ) (Debugger)",
            ValidationDirective::NoValidate,
            Some("debugger"),
            "Fix the bug",
        ),
        // A lone trailing reserved group is the directive (no role hint to claim it).
        (
            "- [ ] [t-004] Fix the bug (no-validate)",
            ValidationDirective::NoValidate,
            None,
            "Fix the bug",
        ),
        (
            "- [ ] [t-005] Fix the bug (validate)",
            ValidationDirective::Validate,
            None,
            "Fix the bug",
        ),
        // A directive on an id-less checklist line still parses.
        (
            "- [ ] Task without an id (no-validate) (validator)",
            ValidationDirective::NoValidate,
            Some("validator"),
            "Task without an id",
        ),
        // No reserved token at all => Auto, grammar unchanged.
        (
            "- [ ] [t-006] Migrate schema (researcher)",
            ValidationDirective::Auto,
            Some("researcher"),
            "Migrate schema",
        ),
        (
            "- [ ] [t-007] Migrate schema",
            ValidationDirective::Auto,
            None,
            "Migrate schema",
        ),
        // Prose parentheticals are never directives — and, with the closed-set
        // role grammar (t-013b), never role hints either: the text stays put.
        (
            "- [ ] [t-008] Write the report (see notes)",
            ValidationDirective::Auto,
            None,
            "Write the report (see notes)",
        ),
        (
            "- [ ] [t-009] Fix the bug (maybe-validate) (coder)",
            ValidationDirective::Auto,
            Some("coder"),
            "Fix the bug (maybe-validate)",
        ),
        (
            "- [ ] [t-010] Draft (draft (v2)) (coder)",
            ValidationDirective::Auto,
            Some("coder"),
            "Draft (draft (v2))",
        ),
        // Emphasis after the trailing groups kills the scan exactly as before.
        (
            "- [ ] [t-012] Fix the bug (no-validate)*",
            ValidationDirective::Auto,
            None,
            "Fix the bug (no-validate)*",
        ),
    ];

    #[test]
    fn validation_directive_before_the_role_hint() {
        for (line, directive, role_hint, body) in DIRECTIVE_FIXTURES {
            let task =
                parse_task_line(line).unwrap_or_else(|| panic!("must be a task line: {line:?}"));
            assert_eq!(
                task.validation_directive, *directive,
                "directive for {line:?}"
            );
            assert_eq!(
                task.role_hint.as_deref(),
                *role_hint,
                "role hint for {line:?}"
            );
            assert_eq!(task.description_body, *body, "body for {line:?}");
            assert_eq!(
                task.description_without_role_hint(),
                *body,
                "body accessor for {line:?}"
            );
            assert_eq!(task.raw, *line, "raw must be byte-identical: {line:?}");
        }
    }

    /// `description` is the verbatim text after the id (directive included);
    /// only `description_body` is stripped, and check-off keeps every byte.
    #[test]
    fn directive_is_stripped_from_the_body_but_raw_and_description_stay_verbatim() {
        let line = "- [ ] [t-014] Investigate library X (no-validate) (researcher)";
        let task = parse_task_line(line).expect("task line");
        assert_eq!(task.validation_directive, ValidationDirective::NoValidate);
        assert_eq!(task.role_hint.as_deref(), Some("researcher"));
        assert_eq!(task.description_body, "Investigate library X");
        assert_eq!(
            task.description,
            "Investigate library X (no-validate) (researcher)"
        );
        assert_eq!(task.raw, line);

        // The token survives check-off byte-for-byte (spans are untouched).
        let flipped = check_off_line(line, "t-014").expect("must flip");
        assert_eq!(
            flipped,
            "- [x] [t-014] Investigate library X (no-validate) (researcher)"
        );
        let reread = parse_task_line(&flipped).expect("checked line still parses");
        assert_eq!(reread.validation_directive, ValidationDirective::NoValidate);
        assert!(reread.is_completed());
    }

    /// Pinned collision: the reversed
    /// spelling is **not** recognised as a directive, and this task deliberately
    /// does not change that (t-013c/t-014a territory).
    ///
    /// t-013b changed only the *collateral* of the pin: with a closed-set role
    /// hint the trailing reserved group is no longer swallowed as
    /// `role_hint == Some("no-validate")` — `(no-validate)` names no archetype, so
    /// the line reports no hint and both groups stay verbatim in the body. The
    /// `Auto` directive below is the pinned part and is byte-for-byte unchanged.
    #[test]
    fn reversed_directive_order_is_not_recognised() {
        let line = "- [ ] [t-013] Investigate library X (researcher) (no-validate)";
        let task = parse_task_line(line).expect("task line");
        assert_eq!(
            task.validation_directive,
            ValidationDirective::Auto,
            "reversed order must still not be recognised as a directive"
        );
        assert_eq!(
            task.role_hint, None,
            "a reserved directive token in the role slot is not an archetype"
        );
        assert_eq!(
            task.description_body,
            "Investigate library X (researcher) (no-validate)"
        );
        assert_eq!(task.raw, line);
    }

    #[test]
    fn directive_lookup_is_a_closed_and_fail_closed_set() {
        assert_eq!(ValidationDirective::default(), ValidationDirective::Auto);
        // In the closed set: exact match, case- and whitespace-insensitive.
        for (token, expected) in [
            ("validate", ValidationDirective::Validate),
            ("Validate", ValidationDirective::Validate),
            ("  validate ", ValidationDirective::Validate),
            ("no-validate", ValidationDirective::NoValidate),
            ("NO-VALIDATE", ValidationDirective::NoValidate),
            ("No-Validate ", ValidationDirective::NoValidate),
        ] {
            assert_eq!(
                ValidationDirective::parse(token),
                expected,
                "'{token}' is in the closed set"
            );
        }
        // Outside the closed set: never `NoValidate`, always `Auto` (fail-closed).
        for outside in [
            "",
            "  ",
            "maybe-validate",
            "no validate",
            "non-validate",
            "unvalidate",
            "(no-validate)",
            "no-validat3",
            "validate now",
            "auto",
            "none",
            "not-validate",
        ] {
            assert_eq!(
                ValidationDirective::parse(outside),
                ValidationDirective::Auto,
                "'{outside}' is outside the closed set and must be Auto (never NoValidate)"
            );
        }
        assert!(ValidationDirective::Auto.is_auto());
        assert!(!ValidationDirective::NoValidate.is_auto());
        assert!(!ValidationDirective::Validate.is_auto());
        // Every spelling of the closed set round-trips through `FromStr`.
        for token in VALIDATION_DIRECTIVES {
            let parsed: ValidationDirective = token.parse().expect("infallible");
            assert_eq!(parsed, ValidationDirective::parse(token));
            assert!(!parsed.is_auto());
        }
        let unknown: ValidationDirective = "no-validat3".parse().expect("infallible");
        assert_eq!(unknown, ValidationDirective::Auto);
    }

    /// Document-level scans must not notice the token at all: ids, check-off and
    /// completion gates stay exactly as they were.
    #[test]
    fn directive_lines_stay_normal_tasks() {
        let plan = "# Plan\n- [ ] [t-020] Fix it (no-validate) (coder)\n- [x] [t-021] Done (validate) (coder)\n";
        assert_eq!(
            unchecked_task_ids(plan),
            vec!["t-020".to_string()],
            "id adjacency must be untouched"
        );
        assert_eq!(
            all_task_ids(plan),
            vec!["t-020".to_string(), "t-021".to_string()]
        );
        let (rewritten, flipped) = check_off_content(plan, "t-020");
        assert!(flipped, "must flip");
        assert!(rewritten.contains("- [x] [t-020] Fix it (no-validate) (coder)"));
        let tasks = parse_tasks(&rewritten);
        assert_eq!(
            tasks
                .iter()
                .map(|t| t.validation_directive)
                .collect::<Vec<_>>(),
            vec![
                ValidationDirective::NoValidate,
                ValidationDirective::Validate,
            ]
        );
    }

    // ── Check-off integrity of directive-bearing lines (t-013c) ─────────────
    //
    // Pinned guarantee: rewriting `- [ ] …` into `- [x] …` splices
    // `checkbox_span` and nothing else, so the reserved `(validate)` /
    // `(no-validate)` token — and every prose group around it — survives
    // byte-for-byte, and the parse of the rewritten line is identical to the
    // parse of the original apart from the checkbox state.

    /// One check-off round-trip case: the plan line, the id that owns it, the
    /// **exact** line expected on the other side of the flip, and the parse both
    /// sides must agree on. Every box in the table is three bytes wide, so a wrong
    /// expectation shows up as a one-character diff.
    struct CheckOffCase {
        line: &'static str,
        task_id: &'static str,
        rewritten: &'static str,
        directive: ValidationDirective,
        role_hint: Option<&'static str>,
        body: &'static str,
    }

    const CHECK_OFF_CASES: &[CheckOffCase] = &[
        // Canonical order: directive before the role hint.
        CheckOffCase {
            line: "- [ ] [t-100] Investigate library X (no-validate) (researcher)",
            task_id: "t-100",
            rewritten: "- [x] [t-100] Investigate library X (no-validate) (researcher)",
            directive: ValidationDirective::NoValidate,
            role_hint: Some("researcher"),
            body: "Investigate library X",
        },
        CheckOffCase {
            line: "- [ ] [t-101] Patch the parser (validate) (coder)",
            task_id: "t-101",
            rewritten: "- [x] [t-101] Patch the parser (validate) (coder)",
            directive: ValidationDirective::Validate,
            role_hint: Some("coder"),
            body: "Patch the parser",
        },
        // The directive is the ONLY trailing group: no role hint claims the slot.
        CheckOffCase {
            line: "- [ ] [t-102] Fix the flake (no-validate)",
            task_id: "t-102",
            rewritten: "- [x] [t-102] Fix the flake (no-validate)",
            directive: ValidationDirective::NoValidate,
            role_hint: None,
            body: "Fix the flake",
        },
        CheckOffCase {
            line: "- [ ] (t-103) Fix the flake (validate)",
            task_id: "t-103",
            rewritten: "- [x] (t-103) Fix the flake (validate)",
            directive: ValidationDirective::Validate,
            role_hint: None,
            body: "Fix the flake",
        },
        // Prose parenthetical + directive + role hint.
        CheckOffCase {
            line: "- [ ] [t-104] Fix the parser (see notes) (no-validate) (researcher)",
            task_id: "t-104",
            rewritten: "- [x] [t-104] Fix the parser (see notes) (no-validate) (researcher)",
            directive: ValidationDirective::NoValidate,
            role_hint: Some("researcher"),
            body: "Fix the parser (see notes)",
        },
        // A nested prose group keeps its own parens intact around the directive.
        CheckOffCase {
            line: "- [ ] [t-105] Draft (draft (v2)) (no-validate) (coder)",
            task_id: "t-105",
            rewritten: "- [x] [t-105] Draft (draft (v2)) (no-validate) (coder)",
            directive: ValidationDirective::NoValidate,
            role_hint: Some("coder"),
            body: "Draft (draft (v2))",
        },
        // Paren checkbox + decorated id + directive + role hint.
        CheckOffCase {
            line: "- ( ) **[t-106]** Ship it (no-validate) (validator)",
            task_id: "t-106",
            rewritten: "- (x) **[t-106]** Ship it (no-validate) (validator)",
            directive: ValidationDirective::NoValidate,
            role_hint: Some("validator"),
            body: "Ship it",
        },
    ];

    /// The line with its checkbox token cut out — literally everything a
    /// check-off is not allowed to touch.
    fn outside_the_checkbox(line: &str, span: (usize, usize)) -> String {
        let (start, end) = span;
        let mut out = String::with_capacity(line.len());
        out.push_str(line.get(..start).expect("checkbox span inside line"));
        out.push_str(line.get(end..).expect("checkbox span inside line"));
        out
    }

    #[test]
    fn check_off_round_trip_keeps_directive_and_role_groups_byte_identical() {
        for case in CHECK_OFF_CASES {
            let line = case.line;
            let tid = case.task_id;
            let task =
                parse_task_line(line).unwrap_or_else(|| panic!("must be a task line: {line:?}"));
            assert!(task.is_pending(), "{line:?} starts unchecked");
            assert_eq!(
                task.validation_directive, case.directive,
                "directive: {line:?}"
            );
            assert_eq!(
                task.role_hint.as_deref(),
                case.role_hint,
                "role hint: {line:?}"
            );
            assert_eq!(task.description_body, case.body, "body: {line:?}");
            assert_eq!(task.raw, line, "raw: {line:?}");

            // The rewrite: exact expected text, never a `contains` match, so a
            // future "re-render the line from its fields" regression fails here.
            let rewritten = check_off_line(line, tid)
                .unwrap_or_else(|| panic!("{tid} must check off its own line: {line:?}"));
            assert_eq!(rewritten, case.rewritten, "rewritten line for {line:?}");
            assert_eq!(
                rewritten.len(),
                line.len(),
                "only the box contents moved: {line:?}"
            );
            let (start, end) = task.checkbox_span;
            // The bracket style of the box is kept: `[ ]` → `[x]`, `( )` → `(x)`.
            let unticked_box = &line[start..end];
            let ticked_box = &rewritten[start..end];
            let expected_box = if unticked_box.starts_with('(') {
                "(x)"
            } else {
                "[x]"
            };
            assert_eq!(ticked_box, expected_box, "box slot of {line:?}");
            assert_eq!(ticked_box.len(), unticked_box.len(), "box width: {line:?}");
            assert_eq!(
                outside_the_checkbox(&rewritten, task.checkbox_span),
                outside_the_checkbox(line, task.checkbox_span),
                "everything outside the checkbox must be byte-identical: {line:?}"
            );

            // Never dropped, never duplicated: one reserved token in, one out.
            for token in VALIDATION_DIRECTIVES {
                let group = format!("({token})");
                assert_eq!(
                    rewritten.matches(group.as_str()).count(),
                    line.matches(group.as_str()).count(),
                    "occurrences of {group} must be unchanged by check-off: {line:?}"
                );
            }

            // Re-reading the rewritten line yields the very same parse, box aside.
            let after = parse_task_line(&rewritten).expect("rewritten line must parse");
            assert!(after.is_completed(), "{line:?} must read as checked");
            assert_eq!(
                after.validation_directive, case.directive,
                "directive survives re-parse: {line:?}"
            );
            assert_eq!(
                after.role_hint.as_deref(),
                case.role_hint,
                "role hint survives re-parse: {line:?}"
            );
            assert_eq!(
                after.description_body, case.body,
                "body survives re-parse: {line:?}"
            );
            assert_eq!(
                after.description, task.description,
                "verbatim description must not move: {line:?}"
            );
            assert_eq!(after.raw, rewritten, "raw of the rewritten line");

            // Idempotent: the checked line is never rewritten a second time.
            assert!(after.checked_line().is_none(), "already checked: {line:?}");
            assert!(
                check_off_line(&rewritten, tid).is_none(),
                "a second check-off must not rewrite {line:?}"
            );
        }
    }

    /// A duplicated reserved token: **exactly one** copy is consumed as the
    /// directive, the other stays verbatim inside `description_body`, and both
    /// stay on disk.
    ///
    /// The rule (see [`split_validation_directive`]): the right-most trailing
    /// group is always reserved for the role-hint slot, and the directive is the
    /// right-most reserved group among the *remaining* ones (scanned right to
    /// left). So in `(no-validate) (no-validate) (coder)` the copy adjacent to
    /// `(coder)` is the directive and the left-hand copy survives in the body;
    /// with no role hint written at all, `(no-validate) (no-validate)` consumes
    /// the **left** copy and keeps the right one (the slot a role hint would
    /// occupy — and since `no-validate` names no archetype, `role_hint` is
    /// `None`, not a bogus hint).
    #[test]
    fn a_duplicated_directive_consumes_exactly_one_copy() {
        let line = "- [ ] [t-110] Ship it (no-validate) (no-validate) (coder)";
        let task = parse_task_line(line).expect("task line");
        assert_eq!(task.validation_directive, ValidationDirective::NoValidate);
        assert_eq!(task.role_hint.as_deref(), Some("coder"));
        assert_eq!(
            task.description, "Ship it (no-validate) (no-validate) (coder)",
            "description keeps every group verbatim"
        );
        assert_eq!(
            task.description_body, "Ship it (no-validate)",
            "one copy consumed, the other kept verbatim"
        );
        assert_eq!(
            check_off_line(line, "t-110").as_deref(),
            Some("- [x] [t-110] Ship it (no-validate) (no-validate) (coder)"),
            "both copies survive check-off in place"
        );
        let after = parse_task_line("- [x] [t-110] Ship it (no-validate) (no-validate) (coder)")
            .expect("rewritten line parses");
        assert_eq!(after.validation_directive, ValidationDirective::NoValidate);
        assert_eq!(after.role_hint.as_deref(), Some("coder"));
        assert_eq!(after.description_body, "Ship it (no-validate)");

        // Two groups only: the right-most stays (it is the role slot), the left
        // one is the directive.
        let two = "- [ ] [t-111] Ship it (no-validate) (no-validate)";
        let task = parse_task_line(two).expect("task line");
        assert_eq!(task.validation_directive, ValidationDirective::NoValidate);
        assert_eq!(
            task.role_hint, None,
            "a reserved token is never a role hint"
        );
        assert_eq!(task.description_body, "Ship it (no-validate)");
        assert_eq!(
            check_off_line(two, "t-111").as_deref(),
            Some("- [x] [t-111] Ship it (no-validate) (no-validate)")
        );
    }

    /// Spans stay truthful in the presence of trailing directive/role groups, and
    /// stay valid across a check-off (the box width is unchanged, so the very same
    /// offsets address the same tokens afterwards).
    #[test]
    fn checkbox_and_id_spans_survive_a_directive_group() {
        for (line, box_text, id) in [
            ("- [ ] [t-120] Fix it (no-validate) (coder)", "[ ]", "t-120"),
            ("- [ ] (t-121) Fix it (validate)", "[ ]", "t-121"),
            (
                "- ( ) [t-122] Paren box (validate) (researcher)",
                "( )",
                "t-122",
            ),
            ("- [ ] **[t-123]** Fix it (no-validate)", "[ ]", "t-123"),
            ("- [X] [t-124] Already done (no-validate)", "[X]", "t-124"),
            (
                "  - [ ] [t-125] Indented (validate) (debugger)",
                "[ ]",
                "t-125",
            ),
        ] {
            let task = parse_task_line(line).unwrap_or_else(|| panic!("task line: {line:?}"));
            let (start, end) = task.checkbox_span;
            assert_eq!(&line[start..end], box_text, "checkbox_span of {line:?}");
            let id_span = task
                .id_span
                .unwrap_or_else(|| panic!("id span for {line:?}"));
            assert_eq!(&line[id_span.0..id_span.1], id, "id_span of {line:?}");
            assert!(start < end, "checkbox span is non-empty: {line:?}");
            assert!(end <= id_span.0, "the id sits after the box: {line:?}");
            assert!(
                id_span.1 <= line.len(),
                "the trailing groups sit right of the id: {line:?}"
            );

            let Some(rewritten) = task.checked_line() else {
                assert!(
                    task.is_completed(),
                    "only a checked line has no rewrite: {line:?}"
                );
                continue;
            };
            assert_eq!(rewritten.len(), line.len(), "box width unchanged: {line:?}");
            let expected_box = if box_text.starts_with('(') {
                "(x)"
            } else {
                "[x]"
            };
            assert_eq!(
                &rewritten[start..end],
                expected_box,
                "same box slot after the flip: {line:?}"
            );
            let after = parse_task_line(&rewritten).expect("rewritten line parses");
            assert_eq!(
                after.checkbox_span, task.checkbox_span,
                "box span moved: {line:?}"
            );
            assert_eq!(after.id_span, Some(id_span), "id span moved: {line:?}");
            assert_eq!(
                after.task_id.as_deref(),
                Some(id),
                "task id moved: {line:?}"
            );
            assert!(
                !after.validation_directive.is_auto(),
                "directive still resolved from the rewritten line: {line:?}"
            );
        }

        // The one rewrite that *does* shift offsets is a wide box (`[  ]` → `[x]`).
        // Spans are byte offsets into `raw`, so the rule is that they are recomputed
        // from the rewritten line — never carried over from the old one.
        let wide = "- [  ]  [t-126]  Fix it (no-validate)";
        let task = parse_task_line(wide).expect("wide box parses");
        assert_eq!(&wide[task.checkbox_span.0..task.checkbox_span.1], "[  ]");
        let rewritten = task.checked_line().expect("unchecked line flips");
        assert_eq!(rewritten, "- [x]  [t-126]  Fix it (no-validate)");
        let after = parse_task_line(&rewritten).expect("rewritten wide line parses");
        assert_eq!(
            &rewritten[after.checkbox_span.0..after.checkbox_span.1],
            "[x]"
        );
        let id_span = after.id_span.expect("id span after the flip");
        assert_eq!(&rewritten[id_span.0..id_span.1], "t-126");
        assert_eq!(after.validation_directive, ValidationDirective::NoValidate);
        assert_eq!(after.description_body, "Fix it");
    }

    /// Document-level scans must see a directive-bearing line as a plain task:
    /// ids, lookup, completion gates and the check-off rewrite all behave exactly
    /// as they do without the token.
    #[test]
    fn document_scans_and_check_off_recognise_directive_lines() {
        let plan = "\
# Execution Plan
- [ ] [t-130] Investigate library X (no-validate) (researcher)
- [ ] (t-131) Patch the parser (validate) (coder)
- [x] [t-132] Baseline captured (no-validate)
- [ ] Fix the flake with no id (validate)
";
        assert_eq!(
            unchecked_task_ids(plan),
            vec!["t-130".to_string(), "t-131".to_string()],
            "id adjacency must be untouched by the token"
        );
        assert_eq!(
            all_task_ids(plan),
            vec![
                "t-130".to_string(),
                "t-131".to_string(),
                "t-132".to_string(),
            ],
            "a checked directive line still reports its id"
        );
        assert_eq!(first_unchecked_line_index(plan), Some(1));
        let found = find_task_line(plan, "t-131").expect("directive line must be findable");
        assert_eq!(found.line_number, 2);
        assert_eq!(found.validation_directive, ValidationDirective::Validate);
        assert_eq!(found.role_hint.as_deref(), Some("coder"));
        assert!(is_pending_task(plan, "t-130"));
        assert!(is_checked_task(plan, "t-132"));
        assert_eq!(
            pending_lines(plan).len(),
            3,
            "the id-less directive box counts too"
        );
        assert_eq!(completed_lines(plan).len(), 1);
        assert!(has_unchecked_box(plan));

        // Exactly one line moves, and it moves byte-for-byte apart from the box.
        let (rewritten, flipped) = check_off_content(plan, "t-131");
        assert!(flipped);
        assert_eq!(
            rewritten,
            "\
# Execution Plan
- [ ] [t-130] Investigate library X (no-validate) (researcher)
- [x] (t-131) Patch the parser (validate) (coder)
- [x] [t-132] Baseline captured (no-validate)
- [ ] Fix the flake with no id (validate)
"
        );
        let reread = find_task_line(&rewritten, "t-131").expect("still findable after check-off");
        assert!(reread.is_completed());
        assert_eq!(reread.validation_directive, ValidationDirective::Validate);
        assert_eq!(reread.role_hint.as_deref(), Some("coder"));
        assert_eq!(reread.description_body, "Patch the parser");

        // Second pass: nothing to flip, and the input is handed back untouched.
        let (again, flipped) = check_off_content(&rewritten, "t-131");
        assert!(
            !flipped,
            "an already checked line must not rewrite the document"
        );
        assert_eq!(again, rewritten);
    }

    /// An already-checked directive line is idempotent at every layer: no rewrite
    /// from `TaskLine::checked_line`, none from `check_off_line`, and
    /// `check_off_content` returns the input byte-for-byte with `flipped == false`.
    #[test]
    fn an_already_checked_directive_line_is_never_rewritten() {
        for line in [
            "- [x] [t-140] Done (validate) (coder)",
            "- [X] (t-141) Done (no-validate)",
            "- (x) [t-142] Done (no-validate) (researcher)",
        ] {
            let task = parse_task_line(line).unwrap_or_else(|| panic!("task line: {line:?}"));
            assert!(task.is_completed(), "{line:?} must read as checked");
            assert!(
                !task.validation_directive.is_auto(),
                "directive read: {line:?}"
            );
            let tid = task.task_id.clone().expect("id on the checked line");
            assert!(
                task.checked_line().is_none(),
                "a ticked box has no rewrite: {line:?}"
            );
            assert!(
                check_off_line(line, &tid).is_none(),
                "check_off_line must refuse an already checked line: {line:?}"
            );
            let (same, flipped) = check_off_content(line, &tid);
            assert!(!flipped, "nothing to flip: {line:?}");
            assert_eq!(
                same, line,
                "an idempotent check-off returns the input verbatim"
            );
            assert_eq!(
                task.description_body, "Done",
                "the body is stripped identically for a checked line: {line:?}"
            );
        }
    }

    /// Byte faithfulness is orthogonal to the directive: CRLF terminators and a
    /// missing trailing newline survive a directive-bearing check-off unchanged.
    #[test]
    fn crlf_and_a_missing_trailing_newline_survive_a_directive_check_off() {
        let crlf =
            "- [ ] [t-150] Fix it (no-validate) (coder)\r\n- [ ] [t-151] Other (validate)\r\n";
        let (rewritten, flipped) = check_off_content(crlf, "t-150");
        assert!(flipped);
        assert_eq!(
            rewritten,
            "- [x] [t-150] Fix it (no-validate) (coder)\r\n- [ ] [t-151] Other (validate)\r\n"
        );

        let bare = "- [ ] (t-152) Ship it (validate)";
        let (rewritten, flipped) = check_off_content(bare, "t-152");
        assert!(flipped);
        assert_eq!(rewritten, "- [x] (t-152) Ship it (validate)");
        assert!(
            !rewritten.ends_with('\n'),
            "no trailing newline was invented"
        );
    }

    /// The same guarantee through the on-disk authority
    /// (`manager::phase::Plan::check_off`, exercised read-only here): the only
    /// bytes that move in the file are the checkbox.
    #[test]
    fn on_disk_check_off_keeps_the_directive_token_on_its_line() {
        let dir = tempfile::tempdir().expect("temp dir");
        let plan = crate::manager::phase::Plan::at(dir.path());
        plan.create(
            "# Execution Plan
- [ ] [t-160] Research library X (no-validate) (researcher)
- [ ] [t-161] Patch the parser (validate) (coder)
",
        )
        .expect("create plan");
        assert_eq!(
            plan.pending_tasks(),
            vec!["t-160".to_string(), "t-161".to_string()],
            "the token must not disturb id discovery on disk"
        );

        assert!(
            plan.check_off("t-160").expect("not an IO error"),
            "must flip"
        );
        let disk = plan.read().expect("read").expect("plan present");
        assert_eq!(
            disk,
            "# Execution Plan
- [x] [t-160] Research library X (no-validate) (researcher)
- [ ] [t-161] Patch the parser (validate) (coder)
"
        );
        // Idempotent second pass: `false`, and the file is byte-identical.
        assert!(
            !plan.check_off("t-160").expect("not an IO error"),
            "already checked"
        );
        assert_eq!(plan.read().expect("read").expect("plan present"), disk);

        let flipped_line = disk
            .lines()
            .find(|line| line.contains("t-160"))
            .expect("t-160 line on disk")
            .to_string();
        let task = parse_task_line(&flipped_line).expect("re-read line parses");
        assert!(task.is_completed());
        assert_eq!(task.validation_directive, ValidationDirective::NoValidate);
        assert_eq!(task.role_hint.as_deref(), Some("researcher"));
        assert_eq!(task.description_body, "Research library X");

        assert!(plan.check_off("t-161").expect("not an IO error"));
        assert!(
            plan.is_complete(),
            "a directive token never blocks completion"
        );
    }

    /// A reserved token written **before** the id (audit case C3) is never turned
    /// into a task id, and this is what the parser then does with the line:
    ///
    /// * `leading_task_id_token` returns `no-validate`, the strict recogniser
    ///   rejects it (it is not `t-`-shaped), so `task_id == None` and
    ///   `id_span == None` — the parser binds nothing rather than inventing an id;
    /// * the line is still a **checkbox line without an id**: it stays out of
    ///   `unchecked_task_ids` / `all_task_ids` / `find_task_line`, and
    ///   `check_off_line` / `check_off_content` can never flip it — while
    ///   `pending_lines` and `has_unchecked_box` still count it, so a plan holding
    ///   such a line stays open until it is re-written in the documented order
    ///   (directive **after** the id, immediately before the role hint);
    /// * the trailing `(coder)` group is still the role hint, but the pre-id group
    ///   is **not** a directive ([`split_validation_directive`] only inspects
    ///   trailing groups) and stays verbatim in `description_body`;
    /// * the UI seam [`task_id_of_line`] recovers `t-170` from its fallback token
    ///   search — which is exactly why id adjacency next to the checkbox is the
    ///   rule, and why a pre-id token must never be accepted as an id.
    #[test]
    fn a_directive_before_the_id_never_becomes_a_task_id() {
        let line = "- [ ] (no-validate) t-170 Do the thing (coder)";
        assert!(
            !is_task_id_token("no-validate"),
            "the reserved token is not id-shaped"
        );
        let task = parse_task_line(line).expect("still a checkbox line");
        assert_eq!(task.task_id, None, "a reserved pre-id token is never an id");
        assert_eq!(task.id_span, None, "no id means no id span");
        assert_eq!(
            task.validation_directive,
            ValidationDirective::Auto,
            "a pre-id group is not a trailing group, so it is not read as a directive"
        );
        assert_eq!(task.role_hint.as_deref(), Some("coder"));
        assert_eq!(task.description, "(no-validate) t-170 Do the thing (coder)");
        assert_eq!(task.description_body, "(no-validate) t-170 Do the thing");
        assert_eq!(&line[task.checkbox_span.0..task.checkbox_span.1], "[ ]");

        let plan = format!("{line}\n- [ ] [t-171] Sibling (no-validate) (coder)\n");
        assert_eq!(
            unchecked_task_ids(&plan),
            vec!["t-171".to_string()],
            "only the canonical line yields an id"
        );
        assert_eq!(all_task_ids(&plan), vec!["t-171".to_string()]);
        assert!(
            find_task_line(&plan, "t-170").is_none(),
            "the pre-id line has no id to find"
        );
        let (same, flipped) = check_off_content(&plan, "t-170");
        assert!(!flipped, "a line with no bound id can never be checked off");
        assert_eq!(same, plan, "and nothing at all is rewritten");
        assert_eq!(
            pending_lines(&plan).len(),
            2,
            "both boxes are still checklist items"
        );
        assert_eq!(first_unchecked_line_index(&plan), Some(0));
        assert!(
            has_unchecked_box(&plan),
            "the plan cannot complete while the line is open"
        );
        assert_eq!(
            task_id_of_line(line).as_deref(),
            Some("t-170"),
            "UI fallback"
        );
        assert!(
            line_matches_task_id(line, "t-170"),
            "UI locator matches the prose id"
        );
    }

    #[test]
    fn strip_list_marker_matches_summary_display_needs() {
        assert_eq!(strip_list_marker("- [ ] [t-001] Task"), "[ ] [t-001] Task");
        assert_eq!(strip_list_marker("1. [x] (t-002) Task"), "[x] (t-002) Task");
        assert_eq!(strip_list_marker("  * [ ] t-003 Task"), "[ ] t-003 Task");
    }

    // Fenced code blocks are documentation, not tasks.

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

    /// Scan of the whole document with fences ignored, for contrast.
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
        // Contrast: with fences ignored the scan advertises a phantom pending
        // task, which would be delegatable and counted in progress.
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
        // Real work still open -> completion stays open because of the REAL line.
        assert!(has_unchecked_box(FENCED_FIXTURE));

        // Same document with the real tasks ticked: the fenced `- [ ]` example
        // must not keep the plan incomplete.
        let done = FENCED_FIXTURE.replace("- [ ] [t-101]", "- [x] [t-101]");
        // Contrast: a whole-document scan sees the fenced box as unchecked,
        // which would keep `Plan::is_complete` false and the plan unarchived.
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

    // Cross-site agreement + on-disk regression guards.

    /// One fixture plan exercising every accepted id spelling, shared by the
    /// agreement tests below.
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

        // A parenthesised id is a real task line for every reader, disk included.
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
