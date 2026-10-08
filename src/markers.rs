//! Single owner of the mission-marker grammar — dedup cluster **C4**
//! (`docs/recon_duplication_helpers.md` §2.4) and the data-integrity bug **C1**
//! (`docs/recon_bugs_manager.md`).
//!
//! Two independent `MissionMarker` types used to exist — one in
//! `src/agents/mod.rs`, one in `src/manager/phase.rs` — each with a verbatim
//! copy of `contains_failed_marker` + `MissionMarker::parse`. Both copies tested
//! `MISSION COMPLETE` as a plain *substring* **before** testing `FAILED`, so a
//! FAILED deliverable that merely *mentions* `MISSION COMPLETE` (subagents echo
//! `MISSION COMPLETE (t-xxx)` inside rejection/rollback prose all the time)
//! was classified `Complete`, which flipped `- [ ] [t-xxx]` → `- [x] [t-xxx]`
//! on disk. This module replaces both copies with one type and one parser.
//!
//! ## Precedence table (authoritative order — do not reorder)
//!
//! | # | Signal found in the deliverable                                        | Result     |
//! |---|--------------------------------------------------------------------|------------|
//! | 1 | structured `Deliverable.marker` field is populated (`resolve`)      | verbatim   |
//! | 2 | `REPLAN REQUIRED` anywhere                                          | `Replan`   |
//! | 3 | an **authoritative** `FAILED`: line-initial, or verdict-shaped      | `Failed`   |
//! |   | (`FAILED: …` / `FAILED (…)` — the shapes the role prompts emit)      |            |
//! | 4 | `MISSION COMPLETE` line-initial or in the terminal summary line,    | `Complete` |
//! |   | and not negated ("I did **not** emit MISSION COMPLETE")              |            |
//! | 5 | any remaining `FAILED` (embedded prose) **and/or** a non-           | `Failed`   |
//! |   | authoritative `MISSION COMPLETE`                                    |            |
//! | 6 | any remaining non-negated `MISSION COMPLETE` (no failure at all)    | `Complete` |
//! | 7 | nothing recognised (incl. a negated-only `MISSION COMPLETE` mention) | `None`     |
//!
//! Rows 3 and 5 are the **FAIL-first rule** and the reason this module exists:
//! an explicit `FAILED` / `REPLAN REQUIRED` signal must win over an *embedded*
//! `MISSION COMPLETE` substring. Rationale — a specialist emits its verdict as
//! its own line (`FAILED: reason`, `FAILED (Validator rejected deliverable)`),
//! so a line-initial or verdict-shaped `FAILED` is positional evidence of the
//! deliverable's outcome, whereas `MISSION COMPLETE` buried in the middle of a
//! sentence is only evidence that those *words* occur in the text. Treating that
//! substring as success corrupts the on-disk plan (`- [ ]` → `- [x]`), hides the
//! task from `pending_tasks()`, and can auto-archive a plan whose work was never
//! done (bug C1). Conversely a line-initial/terminal `MISSION COMPLETE` still
//! beats the word "failed" inside narrative prose (row 4 before row 5), which
//! preserves the historical "narrative mentions an old failure, then completes"
//! behaviour that the validator/fix-loop rounds rely on.
//!
//! ## Vocabulary API — the only sanctioned way to touch a marker elsewhere
//!
//! Gate t-030 found the grammar re-implemented in six production files: ~20
//! hard-coded marker literals, ten of them functional copies of the parser
//! (substring tests, `format!("MISSION COMPLETE ({task_id})")` decorations, a
//! case-sensitive revocation rewrite). Those call sites now call this module:
//!
//! | Helper | Replaces |
//! |---|---|
//! | [`has_complete_marker`] / [`has_replan_marker`] / [`has_failure_marker`] | `upper.contains("<literal>")` |
//! | [`has_terminal_marker`] | the three-way "is this a verdict?" triad |
//! | [`starts_with_complete`] | `first_line.starts_with("<literal>")` |
//! | [`decorated`] / [`failed_trailer`] | the ad-hoc `format!` decorations |
//! | [`revoke_completion`] | the case-sensitive `replace("MISSION COMPLETE", …)` |
//! | [`failure_reason`] / [`FAILURE_VERDICTS`] | hand-parsing `REPLAN REQUIRED` / `FAILED (` / `FAILED:` in the UI |
//! | [`is_failure_verdict_word`] / [`has_failure_verdict_cue`] | the verdict-word chain in `agents::validation` |
//! | [`has_failure_word`] | `first_line.to_lowercase().contains("fail")` in the transcript |
//!
//! `src/markers.rs` is asserted (by `tests` below) to be the only file in
//! `src/` whose production code may contain a marker literal, and the
//! [`MARKER_COMPLETE`] / [`MARKER_FAILED`] / [`MARKER_REPLAN`] constants must
//! have real consumers. The exception list that gate t-030 needed for
//! not-yet-migrated files (`KNOWN_EXTERNAL_MARKER_SITES` in the test module) is
//! a **ratchet**, not an excuse: gate t-059 emptied it and the guard now fails
//! outright on an undocumented or stale entry.

use regex::Regex;
use std::sync::LazyLock;

/// Canonical success marker (REQ-ORCH-005).
pub const MARKER_COMPLETE: &str = "MISSION COMPLETE";
/// Canonical failure marker.
pub const MARKER_FAILED: &str = "FAILED";
/// Canonical "the plan itself must be revisited" marker.
pub const MARKER_REPLAN: &str = "REPLAN REQUIRED";

/// Case-folded forms, matched against the uppercased deliverable text.
const COMPLETE_UPPER: &str = "MISSION COMPLETE";
const FAILED_UPPER: &str = "FAILED";
const REPLAN_UPPER: &str = "REPLAN REQUIRED";

/// Benign failure counters that must not register as a `FAILED` marker
/// (`test result: ok. 15 passed; 0 failed`). Longest first so a shorter
/// phrase can never shadow a longer one.
const BENIGN_FAILURE_COUNTERS: &[&str] = &["0 TESTS FAILED", "0 TEST FAILED", "0 FAILED"];

/// A negation cue in the same sentence immediately before a marker makes that
/// occurrence non-authoritative: "I did **not** emit MISSION COMPLETE",
/// "**without** MISSION COMPLETE", "there is **no** MISSION COMPLETE here".
static NEGATED_BEFORE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\b(?:not|no|never|without|cannot|can\s*not|don'?t|didn'?t|doesn'?t|won'?t|woul'?d\s*not|omit(?:s|ted|ting)?|missing|lacks?|lacking|absent)\b[^.!?\n]{0,30}$",
    )
    .expect("valid negation regex")
});

/// Terminal outcome a specialist returns, and the verdict the plan layer uses
/// to decide whether a plan line may be checked off (REQ-PLAN-002 +
/// REQ-ORCH-005).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionMarker {
    /// Task fully satisfied. `task_id` matches the plan line to auto-check.
    Complete { task_id: Option<String> },
    /// Task could not be completed; report reason + partial result.
    Failed { reason: String },
    /// Task could not be completed AND the plan/goal needs revisiting.
    Replan { reason: String },
}

/// Which marker token an occurrence is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MarkerKind {
    Replan,
    Failed,
    Complete,
}

/// How authoritative an occurrence is, judged by where it sits in the text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Placement {
    /// The marker token is the first non-blank content of its line — the
    /// canonical shape of a terminal marker (`FAILED: …`, `MISSION COMPLETE (t-1)`).
    LineStart,
    /// The marker sits somewhere on the last non-empty line (the terminal
    /// summary line) but not at its start.
    TerminalLine,
    /// The marker is embedded in prose elsewhere — merely a substring mention.
    Embedded,
}

/// One marker occurrence with its positional classification.
#[derive(Debug, Clone, Copy)]
struct Occurrence {
    kind: MarkerKind,
    start: usize,
    end: usize,
    placement: Placement,
    negated: bool,
    /// `FAILED` followed by `:` or `(` — the verdict shapes the role prompts
    /// tell specialists to emit (`FAILED: <reason>`, `FAILED (task incomplete)`).
    verdict_shaped: bool,
}

/// Line offsets plus the index of the last non-empty line ("terminal summary").
struct LineMap {
    starts: Vec<usize>,
    terminal_line: usize,
}

impl MissionMarker {
    /// Parse the terminal marker out of a deliverable's text (REQ-ORCH-005).
    ///
    /// Matching is case-insensitive, but *positional*: markers at the start of
    /// a line or in the terminal summary line outrank substrings buried in
    /// prose, and `FAILED` / `REPLAN REQUIRED` outrank `MISSION COMPLETE`
    /// (see the precedence table in the module docs).
    #[must_use]
    pub fn parse(text: &str) -> Option<MissionMarker> {
        let occurrences = occurrences(text);
        if occurrences.is_empty() {
            return None;
        }

        // Row 2: `REPLAN REQUIRED` outranks everything — a replan is never a
        // success, and it must also win over a `... FAILED` tail (historical
        // behaviour, kept verbatim).
        if find(&occurrences, MarkerKind::Replan).is_some() {
            return Some(MissionMarker::Replan {
                reason: text.to_string(),
            });
        }

        // Row 3 (FAIL-first, part 1): an explicitly emitted failure marker —
        // one that starts its own line, or is verdict-shaped (`FAILED: …` /
        // `FAILED (…)`) — decides the outcome even when the same body also
        // quotes `MISSION COMPLETE`. This is the C1 fix.
        if occurrences.iter().any(|o| {
            o.kind == MarkerKind::Failed
                && (o.placement == Placement::LineStart || o.verdict_shaped)
        }) {
            return Some(MissionMarker::Failed {
                reason: text.to_string(),
            });
        }

        // Row 4: an authoritatively placed, non-negated `MISSION COMPLETE`
        // still beats the word "failed" inside narrative prose.
        let authoritative: Vec<&Occurrence> = occurrences
            .iter()
            .filter(|o| o.kind == MarkerKind::Complete)
            .filter(|o| !o.negated)
            .filter(|o| matches!(o.placement, Placement::LineStart | Placement::TerminalLine))
            .collect();
        if let Some(o) = authoritative
            .iter()
            .min_by_key(|o| placement_rank(o.placement))
        {
            return Some(MissionMarker::Complete {
                task_id: bound_task_id(text, o)
                    .or_else(|| first_bound_task_id(text, &authoritative)),
            });
        }

        // Row 5 (FAIL-first, part 2): neither side produced an authoritative
        // verdict — the only `MISSION COMPLETE` present is embedded prose or a
        // negated mention — so any `FAILED` occurrence, even mid-sentence,
        // decides. Favour the failure: leaving a plan line unchecked costs one
        // extra round, checking it off silently loses the work.
        if find(&occurrences, MarkerKind::Failed).is_some() {
            return Some(MissionMarker::Failed {
                reason: text.to_string(),
            });
        }

        // Row 6: no failure signal whatsoever — keep the historical lenient
        // behaviour and accept a (positionally weaker) completion mention, as
        // long as it is not a negated mention of the marker words.
        let mentions: Vec<&Occurrence> = occurrences
            .iter()
            .filter(|o| o.kind == MarkerKind::Complete && !o.negated)
            .collect();
        if let Some(o) = mentions.first() {
            return Some(MissionMarker::Complete {
                task_id: bound_task_id(text, o).or_else(|| first_bound_task_id(text, &mentions)),
            });
        }

        // Row 7: nothing usable — never auto-check a plan line.
        None
    }

    /// Resolve the authoritative marker for a deliverable.
    ///
    /// The *structured* field wins: when the caller already holds a parsed
    /// `Deliverable.marker`, that verdict is used verbatim and the body text is
    /// never re-parsed (`docs/recon_bugs_manager.md` C1 "minimal fix"). Only
    /// when no structured marker is available does this fall back to the
    /// positional body parse.
    #[must_use]
    pub fn resolve(explicit: Option<&MissionMarker>, body: &str) -> Option<MissionMarker> {
        explicit.cloned().or_else(|| MissionMarker::parse(body))
    }

    /// Returns `true` when the marker is a successful completion, i.e. it
    /// carries `MISSION COMPLETE` (REQ-ORCH-005). Only this marker flips a
    /// plan line to `[x]`; `FAILED` / `REPLAN REQUIRED` never do.
    #[must_use]
    pub fn is_complete(&self) -> bool {
        matches!(self, MissionMarker::Complete { .. })
    }

    /// Returns `true` for `FAILED` / `REPLAN REQUIRED`, i.e. any verdict that
    /// must leave a plan line unchecked.
    #[must_use]
    pub fn is_failure(&self) -> bool {
        matches!(
            self,
            MissionMarker::Failed { .. } | MissionMarker::Replan { .. }
        )
    }
}

/// `true` when already-uppercased text carries a non-benign `FAILED` marker
/// (the sanitiser table used by both historical copies, now owned here).
#[must_use]
pub fn contains_failed_marker(upper: &str) -> bool {
    sanitize_failure_counters(upper).contains(FAILED_UPPER)
}

// ---------------------------------------------------------------------------
// Vocabulary API (gate t-030)
//
// Everything production code is allowed to do with a marker: test for one,
// build one, or rewrite one. These helpers are the *only* sanctioned way to
// touch a marker outside this module, which is what keeps the precedence table
// above single-owner: a call site can never invent its own spelling, its own
// casing rule or its own decoration.
// ---------------------------------------------------------------------------

/// Token written over a completion marker when a deliverable is revoked
/// (`agents::runner::assembly`), replacing the hard-coded `"REVOKED"` spelling.
pub const REVOCATION_TOKEN: &str = "REVOKED";

/// Decorate a marker with the task id it closes:
/// `decorated(MARKER_COMPLETE, "t-007")` -> `MISSION COMPLETE (t-007)`.
///
/// The deliverable assembler and the `delegate_task` handler both append this
/// shape; the spelling lives here so the two sites cannot drift apart.
#[must_use]
pub fn decorated(marker: &str, task_id: &str) -> String {
    format!("{marker} ({task_id})")
}

/// Canonical verdict trailer `FAILED (<context>)`, e.g.
/// `FAILED (Validator rejected deliverable)`.
#[must_use]
pub fn failed_trailer(context: &str) -> String {
    format!("{MARKER_FAILED} ({context})")
}

/// Reason clause of every deliverable that never reached its own verdict: it
/// fills both the `MissionMarker::Failed { reason }` field and the
/// [`failed_trailer`] of the body, so the marker and the text state the same
/// verdict.
pub const ABORT_REASON: &str = "aborted";

/// The single owner of the **"aborted deliverable" body** — the text a worker
/// hands back when its run was cut short before it could state a verdict:
///
/// ```text
/// Task <context>.
///
/// FAILED (aborted)
/// ```
///
/// `context` is the plain-language clause naming *why* the run stopped
/// (`"aborted by user instruction"`, `"execution interrupted or runtime
/// shutting down"`, …); the verdict line is composed by [`failed_trailer`], so
/// the `FAILED`-prefix vocabulary stays owned here and the body always parses
/// back through [`failure_reason`] / [`MissionMarker::parse`].
///
/// Four call sites used to hand-build this body with slightly different wording
/// (`orchestrator::OrchestratorManager::delegate`,
/// `agents::runner::execution`, `agents::Generalist::run`,
/// `orchestrator::delegate`); they all call this one
/// owner now, so an aborted deliverable has exactly one byte-for-byte spelling
/// everywhere — including in the UI, whose failure-reason extraction reads the
/// trailer this writes.
#[must_use]
pub fn aborted_deliverable(context: &str) -> String {
    format!("Task {context}.\n\n{}", failed_trailer(ABORT_REASON))
}

/// `true` when `text` carries the completion marker, in any casing.
#[must_use]
pub fn has_complete_marker(text: &str) -> bool {
    text.to_ascii_uppercase().contains(COMPLETE_UPPER)
}

/// `true` when `text` carries the replan marker, in any casing.
#[must_use]
pub fn has_replan_marker(text: &str) -> bool {
    text.to_ascii_uppercase().contains(REPLAN_UPPER)
}

/// `true` when `text` carries a non-benign `FAILED` marker, in any casing —
/// the same predicate the parser applies in rows 3/5, so a presence test at a
/// call site can never disagree with `MissionMarker::parse` about what counts
/// as a failure (benign `0 failed` test counters stay exempt).
#[must_use]
pub fn has_failure_marker(text: &str) -> bool {
    contains_failed_marker(&text.to_ascii_uppercase())
}

/// `true` when `text` carries *any* terminal marker, in any casing.
///
/// The specialist loop uses this only to decide whether a turn concluded with a
/// verdict at all. It deliberately makes no precedence decision — the verdict
/// itself is always `MissionMarker::parse`.
#[must_use]
pub fn has_terminal_marker(text: &str) -> bool {
    let upper = text.to_ascii_uppercase();
    upper.contains(COMPLETE_UPPER) || upper.contains(REPLAN_UPPER) || contains_failed_marker(&upper)
}

/// `true` when `text` *starts* with the completion marker, in any casing
/// (`mission complete (t-007)` counts). Used by the transcript to summarise a
/// delegated tool result without re-spelling the marker.
#[must_use]
pub fn starts_with_complete(text: &str) -> bool {
    text.get(..COMPLETE_UPPER.len())
        .is_some_and(|head| head.eq_ignore_ascii_case(COMPLETE_UPPER))
}

// ---------------------------------------------------------------------------
// Failure-verdict vocabulary (gate t-059, dedup sweep A / t-044)
//
// The grammar does not stop at "is a marker present". The *shapes* a verdict is
// emitted in (`FAILED (reason)`, `FAILED: reason`, `REPLAN REQUIRED (t-002):
// reason`), the *words* a structured verdict payload carries
// (`{"status": "FAILED"}`), and the weak prose stem that decides whether a line
// reads like a failure, all belong to the same vocabulary. Gate t-044 found
// them re-spelled by hand in `src/agents/validation.rs`, `src/ui/helpers.rs`,
// `src/ui/raw.rs` and `src/ui/transcript.rs`; they live here now, in one table
// each, so the marker owner — and only the marker owner — decides what a
// failure looks like. There is deliberately no second parser module.
// ---------------------------------------------------------------------------

/// Prefix of the parenthesised verdict shape: `FAILED (<reason>)`.
pub const FAILED_PAREN_PREFIX: &str = "FAILED (";
/// Prefix of the colon verdict shape: `FAILED: <reason>`.
pub const FAILED_COLON_PREFIX: &str = "FAILED:";

/// How the reason clause is delimited behind a failure-verdict prefix.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReasonShape {
    /// `FAILED (<reason>)` — the reason is the text up to the first `)`.
    Parenthesis,
    /// `FAILED: <reason>` — the reason is the first non-blank line behind `:`.
    ColonLine,
    /// `REPLAN REQUIRED (<task>): <reason>` — an optional `(...)` group and an
    /// optional `:` are skipped, then the first non-blank line is the reason.
    OptionalParenThenColonLine,
}

/// One failure-verdict prefix together with the shape of its reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FailureVerdict {
    /// The literal prefix to look for (upper-case, matching what the role
    /// prompts instruct specialists to emit).
    pub prefix: &'static str,
    /// Where the reason sits behind that prefix.
    pub shape: ReasonShape,
}

/// The single failure-prefix table, in the precedence order the UI relies on:
/// a replan verdict first (it is the strongest statement), then `FAILED (`,
/// then `FAILED:`. `src/ui/helpers::extract_failure_reason` walks this table
/// instead of hand-parsing the vocabulary.
pub const FAILURE_VERDICTS: &[FailureVerdict] = &[
    FailureVerdict {
        prefix: MARKER_REPLAN,
        shape: ReasonShape::OptionalParenThenColonLine,
    },
    FailureVerdict {
        prefix: FAILED_PAREN_PREFIX,
        shape: ReasonShape::Parenthesis,
    },
    FailureVerdict {
        prefix: FAILED_COLON_PREFIX,
        shape: ReasonShape::ColonLine,
    },
];

/// The failure-prefix table, exposed so a caller can render or assert the
/// vocabulary without copying it.
#[must_use]
pub const fn failure_verdicts() -> &'static [FailureVerdict] {
    FAILURE_VERDICTS
}

/// Extract the reason clause carried by the first failure verdict in `text`,
/// walking [`FAILURE_VERDICTS`] in table order.
///
/// `None` means "no marker-shaped reason here" — the caller decides what its
/// non-marker fallback is (the UI falls back to `ERROR:`, abort lines and the
/// first non-blank line). Presentation limits (truncation, ellipsis) stay with
/// the caller; this returns the raw trimmed clause as a subslice of the input.
///
/// Matching is **case-sensitive by design**: these are the exact shapes the
/// role prompts emit and the shapes pinned by the UI's output tests. The
/// case-insensitive question ("does this text contain a failure marker at
/// all?") is [`has_failure_marker`], and the authoritative verdict is always
/// [`MissionMarker::parse`] — this function only recovers the reason text of a
/// verdict that is already spelled in a canonical shape.
#[must_use]
pub fn failure_reason(text: &str) -> Option<&str> {
    for verdict in FAILURE_VERDICTS {
        let Some(pos) = text.find(verdict.prefix) else {
            continue;
        };
        let after = &text[pos + verdict.prefix.len()..];
        let candidate = match verdict.shape {
            ReasonShape::Parenthesis => after.find(')').map(|close| &after[..close]),
            ReasonShape::ColonLine => Some(first_non_empty_line(after)),
            ReasonShape::OptionalParenThenColonLine => {
                let after = after.trim_start();
                let after = if after.starts_with('(') {
                    match after.find(')') {
                        Some(close) => &after[close + 1..],
                        None => after,
                    }
                } else {
                    after
                };
                Some(first_non_empty_line(
                    after.strip_prefix(':').unwrap_or(after),
                ))
            }
        };
        if let Some(reason) = candidate.map(str::trim).filter(|reason| !reason.is_empty()) {
            return Some(reason);
        }
    }
    None
}

/// The first non-blank line of `text` (`""` when there is none).
fn first_non_empty_line(text: &str) -> &str {
    text.lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
}

/// Verdict words that state a **negative** verdict when they are the whole
/// value of a verdict field (`{"status": "FAILED"}`,
/// `{"decision": "DECLINED"}`). Compared case-insensitively against the
/// trimmed field value; this is the table `agents::validation` used to
/// hand-roll as a chain of `eq_ignore_ascii_case("<literal>")` calls.
pub const FAILURE_VERDICT_WORDS: &[&str] = &[
    "REJECTED",
    "REJECT",
    "FAIL",
    "FAILED",
    "DECLINED",
    "DECLINE",
    "DISAPPROVED",
];

/// `true` when `value` is one of [`FAILURE_VERDICT_WORDS`] (case-insensitive,
/// whole-value comparison).
#[must_use]
pub fn is_failure_verdict_word(value: &str) -> bool {
    FAILURE_VERDICT_WORDS
        .iter()
        .any(|word| value.eq_ignore_ascii_case(word))
}

/// Substring cues that an *unstructured* comment carries an explicit
/// failure/rejection verdict, matched against already-uppercased text
/// (uppercase the haystack once, then ask this).
///
/// Deliberately separate from [`FAILURE_VERDICT_WORDS`]: a verdict *field* is
/// matched as a whole word, free-form *prose* is matched as a substring, and
/// conflating the two would change validator behaviour.
pub const FAILURE_PROSE_CUES: &[&str] = &["REJECT", "FAILED", "FAILURE"];

/// `true` when already-uppercased `upper` carries any of
/// [`FAILURE_PROSE_CUES`].
#[must_use]
pub fn has_failure_verdict_cue(upper: &str) -> bool {
    FAILURE_PROSE_CUES.iter().any(|cue| upper.contains(cue))
}

/// Lower-cased failure *stems* — weaker than [`has_failure_marker`], which
/// demands the whole `FAILED` token. Used only where the question is "does this
/// line read like a failure?" (the transcript's one-line summary of a delegated
/// tool result), never for a verdict.
pub const FAILURE_WORD_STEMS: &[&str] = &["fail"];

/// `true` when `text` contains one of [`FAILURE_WORD_STEMS`].
///
/// Case folding uses `to_lowercase` (Unicode) rather than `to_ascii_lowercase`
/// because the historical call site did, and the transcript's rendered summary
/// must not change: some full-width capitals (ｅｆｇｉ) fold into ASCII stems.
#[must_use]
pub fn has_failure_word(text: &str) -> bool {
    let lower = text.to_lowercase();
    FAILURE_WORD_STEMS.iter().any(|stem| lower.contains(stem))
}

/// Rewrite every occurrence of the completion marker to [`REVOCATION_TOKEN`],
/// **case-correctly** (`MISSION COMPLETE`, `mission complete`,
/// `Mission Complete`, `Mission COMPLETE`, …).
///
/// The historical caller did two case-sensitive `replace()` calls, so any other
/// casing survived a rejection: the rejected deliverable kept an intact
/// completion marker, which `MissionMarker::parse` reads case-insensitively and
/// which therefore still checked `- [ ] [t-xxx]` off on disk. Revocation now
/// reuses the parser's own case-insensitive occurrence scan
/// ([`find_all`] over the ascii-uppercased text) instead of a second parser.
///
/// Byte offsets transfer 1:1 because `to_ascii_uppercase` is length-preserving
/// and ASCII bytes never occur inside a multi-byte UTF-8 sequence, so the
/// rewrite cannot split a character.
#[must_use]
pub fn revoke_completion(text: &str) -> String {
    let upper = text.to_ascii_uppercase();
    let mut out = text.to_string();
    // Reverse order: every range to be replaced starts after the ones already
    // rewritten, so earlier offsets stay valid.
    for (start, end) in find_all(&upper, COMPLETE_UPPER).into_iter().rev() {
        out.replace_range(start..end, REVOCATION_TOKEN);
    }
    out
}

/// Blank the benign failure counters *in place by width* so their `FAILED`
/// text stops registering as a marker while every other byte offset stays
/// aligned with the input (positions are used for the precedence rules).
fn sanitize_failure_counters(upper: &str) -> String {
    let mut out = upper.to_string();
    for phrase in BENIGN_FAILURE_COUNTERS {
        while let Some(pos) = out.find(phrase) {
            out.replace_range(pos..pos + phrase.len(), &" ".repeat(phrase.len()));
        }
    }
    out
}

/// All marker occurrences in `text`, classified by placement and negation.
fn occurrences(text: &str) -> Vec<Occurrence> {
    let upper = text.to_ascii_uppercase();
    let hay = sanitize_failure_counters(&upper);
    let map = line_map(text);

    let mut out = Vec::new();
    for (kind, token) in [
        (MarkerKind::Replan, REPLAN_UPPER),
        (MarkerKind::Failed, FAILED_UPPER),
        (MarkerKind::Complete, COMPLETE_UPPER),
    ] {
        for (start, end) in find_all(&hay, token) {
            out.push(Occurrence {
                kind,
                start,
                end,
                placement: placement_of(start, &map, &hay),
                negated: negated_before(&hay, start),
                verdict_shaped: kind == MarkerKind::Failed && verdict_shaped(&hay, end),
            });
        }
    }
    out
}

fn find(occurrences: &[Occurrence], kind: MarkerKind) -> Option<&Occurrence> {
    occurrences.iter().find(|o| o.kind == kind)
}

const fn placement_rank(placement: Placement) -> u8 {
    match placement {
        Placement::LineStart => 0,
        Placement::TerminalLine => 1,
        Placement::Embedded => 2,
    }
}

fn find_all(hay: &str, needle: &str) -> Vec<(usize, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(rel) = hay[from..].find(needle) {
        let start = from + rel;
        let end = start + needle.len();
        out.push((start, end));
        from = end;
    }
    out
}

/// Byte offsets of every line start, plus the last non-empty line index.
fn line_map(text: &str) -> LineMap {
    let mut starts = vec![0usize];
    for (idx, byte) in text.bytes().enumerate() {
        if byte == b'\n' {
            starts.push(idx + 1);
        }
    }
    let mut terminal_line = 0;
    for (idx, &line_start) in starts.iter().enumerate() {
        let line_end = starts.get(idx + 1).copied().unwrap_or(text.len());
        if text[line_start..line_end].trim().is_empty() {
            continue;
        }
        terminal_line = idx;
    }
    LineMap {
        starts,
        terminal_line,
    }
}

/// Classify where `pos` sits. `sanitized` has the same length as the input, so
/// its line map and offsets are interchangeable with the original text.
fn placement_of(pos: usize, map: &LineMap, sanitized: &str) -> Placement {
    let idx = match map.starts.binary_search(&pos) {
        Ok(i) => i,
        Err(i) => i.saturating_sub(1),
    };
    let line_start = map.starts[idx];
    if sanitized[line_start..pos].trim().is_empty() {
        return Placement::LineStart;
    }
    if idx == map.terminal_line {
        return Placement::TerminalLine;
    }
    Placement::Embedded
}

/// `true` when a negation cue directly precedes the marker occurrence.
fn negated_before(sanitized: &str, start: usize) -> bool {
    let mut win = start.saturating_sub(64);
    while win < start && !sanitized.is_char_boundary(win) {
        win += 1;
    }
    NEGATED_BEFORE_RE.is_match(&sanitized[win..start])
}

/// `true` when the `FAILED` token ending at `end` is immediately followed by
/// `:` or `(`, i.e. it is one of the verdict shapes the role prompts instruct
/// specialists to emit (`FAILED: <reason>`, `FAILED (Validator rejected …)`)
/// rather than the word "failed" inside a sentence.
fn verdict_shaped(sanitized: &str, end: usize) -> bool {
    sanitized[end..]
        .trim_start()
        .chars()
        .next()
        .is_some_and(|c| c == ':' || c == '(')
}

/// Task id for a completion occurrence: the task-id token sitting on the
/// marker's own line, after the marker words (`MISSION COMPLETE (t-007)`).
///
/// Two hardening rules, from the H6 hand-off after t-031d
/// (`docs/recon_bugs_manager.md` H6):
///
/// 1. **A token, never a substring.** The match is delegated to
///    [`crate::plan_parse::find_task_id_token`], the crate's single task-id
///    token recogniser, which is boundary-aware. This module used to own a
///    private copy of the task-id regex that was *not* boundary-safe, so a
///    deliverable mentioning a path such as `src/chart-notes.md` bound the
///    task `t-notes` and checked off work nobody had done.
/// 2. **The marker's own line only.** The old second half of this function
///    scanned the WHOLE deliverable for the first `t-…` run; that whole-text
///    fallback is removed. An id named anywhere else in the text is prose, not
///    the verdict's id — callers that know the id bind it explicitly (the
///    `delegate_task` argument, or the structured [`MissionMarker`] field).
fn bound_task_id(text: &str, o: &Occurrence) -> Option<String> {
    let line_end = text[o.end..]
        .find('\n')
        .map_or(text.len(), |rel| o.end + rel);
    let mut window_end = line_end.min(o.end + 120).max(o.end);
    while window_end > o.start && !text.is_char_boundary(window_end) {
        window_end -= 1;
    }
    crate::plan_parse::find_task_id_token(&text[o.start..window_end])
}

/// Last-resort id lookup for a completion verdict: the first task-id token that
/// sits on **the own line of any non-negated `MISSION COMPLETE` occurrence** in
/// `candidates`.
///
/// This is deliberately NOT a text scan (H6): every candidate is a line that
/// actually spells the completion marker, and the id still has to be a
/// boundary-safe token on that line. It exists for the shape the harness
/// produces — `agents::runner::assembly::assemble_final_deliverable` appends a
/// decorated `MISSION COMPLETE (t-xxx)` when the deliverable already ended with
/// a bare marker — where the authoritative (first) occurrence happens to be the
/// undecorated one. It changes no precedence decision: the verdict kind and the
/// occurrence-authority ranking are untouched, only which marker line hands out
/// the id.
fn first_bound_task_id(text: &str, candidates: &[&Occurrence]) -> Option<String> {
    candidates.iter().find_map(|o| bound_task_id(text, o))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Row 4: line-initial completion, id bound to the marker.
    #[test]
    fn parses_line_initial_completion_with_bound_task_id() {
        assert_eq!(
            MissionMarker::parse("MISSION COMPLETE (t-001)"),
            Some(MissionMarker::Complete {
                task_id: Some("t-001".to_string())
            })
        );
        // Completion in the terminal summary line still counts.
        assert!(
            MissionMarker::parse("all done MISSION COMPLETE (t-501)")
                .is_some_and(|m| m.is_complete())
        );
        // A completion without any id token is still a completion.
        assert_eq!(
            MissionMarker::parse("MISSION COMPLETE without an id"),
            Some(MissionMarker::Complete { task_id: None })
        );
    }

    // ---- H6 hardening: the task id bound to a marker is a *token*, never a
    // substring. These three guards pin the fix for the hand-off recorded after
    // t-031d: the module-private task-id regex was not word-boundary safe, and
    // `bound_task_id` fell back to scanning the WHOLE deliverable, so a task id
    // hiding inside a path could be bound to a completion marker — the
    // `docs/recon_bugs_manager.md` H6 failure mode, one layer below the
    // `manager::phase` caller-side guard.

    /// (i) A path whose name contains a `t-…` substring must never bind that
    /// substring as the marker's task id: `src/chart-notes.md` → `t-notes` is
    /// rejected because the `t-…` run starts mid-word, i.e. it is not a
    /// boundary-safe task-id token.
    #[test]
    fn path_substring_on_the_marker_line_is_never_a_bound_task_id() {
        let fixture = "Notes consolidated.\n\nMISSION COMPLETE — see src/chart-notes.md";
        // Witness: the deleted unbounded pattern really did bind the path
        // substring, i.e. the guards below are not vacuous.
        assert_eq!(
            legacy_unbounded_task_id("MISSION COMPLETE — see src/chart-notes.md").as_deref(),
            Some("t-notes"),
            "witness must reproduce the H6 mis-derivation"
        );
        for text in [
            fixture,
            "MISSION COMPLETE (see src/chart-notes.md)",
            "work done\n\nMISSION COMPLETE — chart-notes.md and src/chart-tests.md agree",
        ] {
            assert_eq!(
                MissionMarker::parse(text),
                Some(MissionMarker::Complete { task_id: None }),
                "path substring must not bind a task id: {text:?}"
            );
        }
    }

    /// (ii) A legitimately decorated marker still binds its own id — round,
    /// square and undecorated spellings, in the marker's own line.
    #[test]
    fn decorated_marker_ids_still_bind() {
        for (text, id) in [
            ("MISSION COMPLETE (t-021)", "t-021"),
            ("report\n\nMISSION COMPLETE [t-100a1]", "t-100a1"),
            ("done\n\nMISSION COMPLETE t-7", "t-7"),
            ("MISSION COMPLETE (t-c3ui-2): two files touched", "t-c3ui-2"),
        ] {
            assert_eq!(
                MissionMarker::parse(text),
                Some(MissionMarker::Complete {
                    task_id: Some(id.to_string())
                }),
                "decorated marker must bind its id: {text:?}"
            );
        }
    }

    /// (iii) An id named in prose somewhere other than the marker's own line
    /// must not bind: the whole-text fallback is gone, so only the marker line
    /// can hand out an id.
    #[test]
    fn prose_id_on_another_line_is_not_bound() {
        assert_eq!(
            MissionMarker::parse("Supersedes t-006; see also t-013.\n\nMISSION COMPLETE"),
            Some(MissionMarker::Complete { task_id: None })
        );
        // The id may even sit on the same *paragraph* but not on the marker
        // line — still not bound.
        assert_eq!(
            MissionMarker::parse("MISSION COMPLETE\n\nt-006 is the task this covers."),
            Some(MissionMarker::Complete { task_id: None })
        );
    }

    /// A bare completion marker followed by a decorated one still binds the id
    /// named on the decorated marker's own line — the shape
    /// `agents::runner::assembly::assemble_final_deliverable` produces when a
    /// deliverable already ended with a bare `MISSION COMPLETE` and the harness
    /// appends the decorated token. Still strictly marker-line anchored: an id
    /// named on neither marker line stays unbound.
    #[test]
    fn decorated_marker_after_a_bare_one_still_binds() {
        assert_eq!(
            MissionMarker::parse("Done.\n\nMISSION COMPLETE\n\nMISSION COMPLETE (t-031)"),
            Some(MissionMarker::Complete {
                task_id: Some("t-031".to_string())
            })
        );
        assert_eq!(
            MissionMarker::parse("see t-031\n\nMISSION COMPLETE\n\nMISSION COMPLETE"),
            Some(MissionMarker::Complete { task_id: None }),
            "the same-kind fallback must not become a whole-text scan"
        );
    }

    /// Row 2: `REPLAN REQUIRED` is never a success and outranks `FAILED`.
    #[test]
    fn replan_outranks_failed_and_never_succeeds() {
        assert!(matches!(
            MissionMarker::parse("REPLAN REQUIRED: missing deps"),
            Some(MissionMarker::Replan { .. })
        ));
        assert!(
            MissionMarker::parse("REPLAN REQUIRED ... FAILED anyway")
                .is_some_and(|m| matches!(m, MissionMarker::Replan { .. }))
        );
        // A replan signal outranks an authoritative completion marker too.
        assert!(
            MissionMarker::parse("MISSION COMPLETE (t-003)\n\nREPLAN REQUIRED: schema change")
                .is_some_and(|m| m.is_failure())
        );
    }

    /// Rows 3/6: explicit and embedded failure markers.
    #[test]
    fn parses_failure_markers() {
        assert!(
            MissionMarker::parse("FAILED because of test errors").is_some_and(|m| m.is_failure())
        );
        assert!(
            MissionMarker::parse("Compilation error: FAILED to compile src/main.rs")
                .is_some_and(|m| matches!(m, MissionMarker::Failed { .. }))
        );
        assert!(
            MissionMarker::parse("could not finish: FAILED because of x")
                .is_some_and(|m| m.is_failure())
        );
    }

    /// Row 4 before row 6: narrative talk about an *old* failure must not
    /// veto a genuine, authoritatively placed completion.
    #[test]
    fn completion_beats_failure_words_in_narrative() {
        let narrative =
            "Previous build failed with syntax error. Fixed now.\n\nMISSION COMPLETE (t-001)";
        assert_eq!(
            MissionMarker::parse(narrative),
            Some(MissionMarker::Complete {
                task_id: Some("t-001".to_string())
            })
        );
        assert!(
            MissionMarker::parse("Previous attempt failed with error. MISSION COMPLETE (t-008)")
                .is_some_and(|m| m.is_complete())
        );
    }

    /// Benign counters (`0 failed`, `0 tests failed`) never register as a
    /// failure marker.
    #[test]
    fn benign_failure_counters_are_exempt() {
        assert_eq!(
            MissionMarker::parse("test result: ok. 15 passed; 0 failed; 0 ignored"),
            None
        );
        assert!(
            MissionMarker::parse(
                "test result: ok. 15 passed; 0 failed; 0 ignored\n\nMISSION COMPLETE (t-002)"
            )
            .is_some_and(|m| m.is_complete())
        );
        assert!(!contains_failed_marker("15 PASSED; 0 FAILED"));
        assert!(contains_failed_marker("BUILD FAILED"));
    }

    /// A verdict-shaped `FAILED` (`FAILED:` / `FAILED (`) outranks a quoted
    /// completion mention even when it is not line-initial.
    #[test]
    fn verdict_shaped_failure_outranks_a_quoted_completion() {
        let deliverable = "Round 3 report: MISSION COMPLETE was echoed by the \
                           worker, but the run FAILED: build errors remain";
        assert!(
            matches!(
                MissionMarker::parse(deliverable),
                Some(MissionMarker::Failed { .. })
            ),
            "expected Failed"
        );
    }

    // ---- regression guards for bug C1 (docs/recon_bugs_manager.md) ----------

    /// Witness of the task-id grammar this module **used to own**
    /// `\(?\[?(t-[A-Za-z0-9_-]+)\]?` with an optional fallback scan over the
    /// whole text, kept under `cfg(test)` only (same convention as
    /// `manager::phase_tests`'s legacy witnesses). It is deliberately not
    /// boundary-safe: that is exactly what bound `t-notes` out of
    /// `src/chart-notes.md`.
    fn legacy_unbounded_task_id(text: &str) -> Option<String> {
        regex::Regex::new(r"\(?\[?(t-[A-Za-z0-9_-]+)\]?\)?")
            .expect("witness regex")
            .captures(text)
            .map(|c| c[1].to_string())
    }

    /// The parser that used to live in `src/agents/mod.rs:122-136` and
    /// (character-for-character) in `src/manager/phase.rs:222-243`, kept here
    /// under `cfg(test)` only as a witness: it tests `MISSION COMPLETE` as a
    /// substring *before* `FAILED`, so it cannot see that a failure verdict and
    /// a quoted completion mention are different things.
    fn legacy_substring_first_parse(text: &str) -> Option<MissionMarker> {
        let upper = text.to_ascii_uppercase();
        if upper.contains("REPLAN REQUIRED") {
            return Some(MissionMarker::Replan {
                reason: text.to_string(),
            });
        }
        if upper.contains("MISSION COMPLETE") {
            let task_id = legacy_unbounded_task_id(text);
            return Some(MissionMarker::Complete { task_id });
        }
        if contains_failed_marker(&upper) {
            return Some(MissionMarker::Failed {
                reason: text.to_string(),
            });
        }
        None
    }

    /// Proof that regression test (a) is live: the deleted substring-first
    /// parser reports the very same deliverable as `Complete { t-014 }` — which
    /// is what flipped `- [ ] [t-014]` to `- [x]` on disk — while the
    /// single-owner parser reports `Failed`.
    #[test]
    fn regression_old_substring_first_parser_misclassified_case_a_as_complete() {
        let deliverable = "FAILED: replace tool rejected; I did not emit \
                           MISSION COMPLETE (t-014) because the build broke";
        let old = legacy_substring_first_parse(deliverable).expect("old marker");
        assert!(
            matches!(old, MissionMarker::Complete { .. }),
            "the old parser must reproduce the bug: {old:?}"
        );
        let now = MissionMarker::parse(deliverable).expect("new marker");
        assert!(
            matches!(now, MissionMarker::Failed { .. }),
            "the shared parser must classify it Failed, got {now:?}"
        );
    }

    /// (a) A FAILED deliverable whose body *mentions* `MISSION COMPLETE` must
    /// classify as FAILED — the old substring-first parser said `Complete`.
    #[test]
    fn regression_failed_body_mentioning_mission_complete_is_failed() {
        let deliverable = "FAILED: replace tool rejected; I did not emit \
                           MISSION COMPLETE (t-014) because the build broke";
        let marker = MissionMarker::parse(deliverable).expect("a marker");
        assert!(
            matches!(marker, MissionMarker::Failed { .. }),
            "must be Failed, got {marker:?}"
        );
        assert!(!marker.is_complete());
    }

    /// (a) variant: the marker does not even have to be line-initial — a plain
    /// substring `MISSION COMPLETE` inside a failed deliverable is not proof
    /// of success.
    #[test]
    fn regression_embedded_mission_complete_in_failed_body_is_not_success() {
        let deliverable = "Rollback performed. The earlier round echoed \
                           MISSION COMPLETE (t-021) but that revision was rejected.\n\
                           FAILED (Validator rejected deliverable)";
        assert!(
            MissionMarker::parse(deliverable)
                .is_some_and(|m| matches!(m, MissionMarker::Failed { .. }))
        );
    }

    /// (b) A genuine `MISSION COMPLETE (t-007)` deliverable still parses as a
    /// completion carrying the bound task id.
    #[test]
    fn regression_genuine_completion_still_parses() {
        assert_eq!(
            MissionMarker::parse(
                "Wrote src/markers.rs and ran the suite.\n\nMISSION COMPLETE (t-007)"
            ),
            Some(MissionMarker::Complete {
                task_id: Some("t-007".to_string())
            })
        );
    }

    /// (d) The structured `Deliverable.marker` field wins over body text.
    #[test]
    fn regression_structured_marker_wins_over_body_text() {
        let explicit = MissionMarker::Complete {
            task_id: Some("t-005".to_string()),
        };
        let body = "the previous attempt FAILED, this one is fine";
        assert_eq!(
            MissionMarker::resolve(Some(&explicit), body),
            Some(explicit.clone())
        );

        // The inverse also holds: an explicit FAILED is not rescued by a
        // `MISSION COMPLETE` body.
        let explicit_failed = MissionMarker::Failed {
            reason: "validator rejected".to_string(),
        };
        assert_eq!(
            MissionMarker::resolve(Some(&explicit_failed), "MISSION COMPLETE (t-006)"),
            Some(explicit_failed)
        );

        // No structured marker -> positional body parse.
        assert_eq!(
            MissionMarker::resolve(None, "MISSION COMPLETE (t-008)"),
            Some(MissionMarker::Complete {
                task_id: Some("t-008".to_string())
            })
        );
    }

    /// Negated mentions of the success marker are non-authoritative: they must
    /// never be reported as a completion.
    #[test]
    fn negated_completion_mentions_are_not_authoritative() {
        assert_eq!(
            MissionMarker::parse("Status update: no MISSION COMPLETE here yet."),
            None
        );
        assert_eq!(
            MissionMarker::parse("I did not emit MISSION COMPLETE."),
            None
        );
        assert!(
            MissionMarker::parse(
                "He said MISSION COMPLETE but never wrote it.\n\nFAILED (no work)"
            )
            .is_some_and(|m| matches!(m, MissionMarker::Failed { .. }))
        );
    }

    /// Nothing recognised -> `None` (never auto-check a plan line).
    #[test]
    fn unrecognised_text_yields_no_marker() {
        assert_eq!(MissionMarker::parse("just a status update"), None);
        assert_eq!(MissionMarker::parse(""), None);
    }

    // ---- vocabulary API (gate t-030) --------------------------------------

    /// Presence tests are case-correct — the historical call sites each wrote
    /// their own `upper.contains("<literal>")`, and one of them (the revocation
    /// rewrite) forgot to fold case at all.
    #[test]
    fn vocabulary_presence_tests_are_case_correct() {
        for text in [
            "MISSION COMPLETE",
            "mission complete",
            "Mission Complete",
            "mIsSiOn CoMpLeTe (t-1)",
        ] {
            assert!(has_complete_marker(text), "{text}");
            assert!(starts_with_complete(text), "{text}");
            assert!(!has_replan_marker(text), "{text}");
        }
        assert!(!starts_with_complete(
            "we report MISSION COMPLETE at the end"
        ));
        assert!(starts_with_complete("mission complete — done"));
        // Non-ASCII prefix must not panic on a byte boundary.
        assert!(!starts_with_complete("éè MISSION COMPLETE"));

        for text in ["REPLAN REQUIRED", "replan required", "Replan Required"] {
            assert!(has_replan_marker(text), "{text}");
        }
        for text in ["FAILED", "failed", "FaiLed: build broke"] {
            assert!(has_failure_marker(text), "{text}");
        }
        // The benign test-counter table stays exempt here too, so a presence
        // test can never disagree with the parser.
        assert!(!has_failure_marker("test result: ok. 15 passed; 0 failed"));
        assert!(!has_failure_marker("0 tests failed"));

        assert!(has_terminal_marker("all good, Mission Complete"));
        assert!(has_terminal_marker("Replan Required: schema change"));
        assert!(has_terminal_marker("the build FaiLed"));
        assert!(!has_terminal_marker("no verdict, just progress notes"));
        assert!(!has_terminal_marker("test result: ok. 3 passed; 0 failed"));
    }

    /// The decoration/trailer spellings are owned here, not by the callers.
    #[test]
    fn decoration_and_trailer_come_from_the_constants() {
        assert_eq!(
            decorated(MARKER_COMPLETE, "t-042"),
            "MISSION COMPLETE (t-042)"
        );
        assert_eq!(decorated(MARKER_REPLAN, "t-7"), "REPLAN REQUIRED (t-7)");
        assert_eq!(
            failed_trailer("Validator rejected deliverable"),
            "FAILED (Validator rejected deliverable)"
        );
        // Whatever the caller decorates, the parser reads it back unchanged.
        assert_eq!(
            MissionMarker::parse(&decorated(MARKER_COMPLETE, "t-042")),
            Some(MissionMarker::Complete {
                task_id: Some("t-042".to_string())
            })
        );
    }

    /// Regression (gate t-030): the revocation rewrite was two case-sensitive
    /// `replace()` calls, so every casing except two survived. `MissionMarker`
    /// is case-insensitive, so a rejected/aborted deliverable kept a live
    /// completion marker and the plan line was still checked off.
    #[test]
    fn revoke_completion_is_case_correct() {
        let legacy: Vec<String> = vec![
            MARKER_COMPLETE.to_string(),
            MARKER_COMPLETE.to_lowercase(),
            "Mission Complete".to_string(),
            "Mission COMPLETE".to_string(),
            "mIsSiOn CoMpLeTe".to_string(),
        ];
        for variant in &legacy {
            let revoked = revoke_completion(&format!("report\n\n{variant} (t-023)"));
            assert!(
                !has_complete_marker(&revoked),
                "{variant} survived revocation: {revoked}"
            );
            assert!(revoked.contains(REVOCATION_TOKEN), "{variant} -> {revoked}");
            assert!(revoked.contains("(t-023)"), "task id lost: {revoked}");
            // A revoked marker is no longer a completion for the parser.
            assert!(
                !MissionMarker::parse(&revoked).is_some_and(|m| m.is_complete()),
                "parsed as complete after revocation: {revoked}"
            );
        }
    }

    /// Revocation rewrites *every* occurrence, keeps byte offsets intact for
    /// multi-byte text, and never touches the other markers.
    #[test]
    fn revoke_completion_rewrites_all_occurrences_only() {
        let text = "Mission Complete ✅ (t-1) and mission complete again ✅\nFAILED stays";
        let revoked = revoke_completion(text);
        assert_eq!(
            revoked,
            format!("{REVOCATION_TOKEN} ✅ (t-1) and {REVOCATION_TOKEN} again ✅\nFAILED stays")
        );
        assert!(revoked.contains("FAILED stays"));
        assert_eq!(revoke_completion("no marker at all"), "no marker at all");
        assert_eq!(revoke_completion(""), "");
    }

    // ---- failure-verdict vocabulary (gate t-059) ---------------------------

    /// The failure-prefix table must stay pinned to the marker constants: a
    /// prefix that drifts from `MARKER_FAILED` / `MARKER_REPLAN` would create a
    /// second, invisible spelling of the grammar (and would slip past the
    /// single-owner guard, which scans for the marker literals themselves).
    #[test]
    fn failure_verdict_prefixes_are_pinned_to_the_marker_constants() {
        assert_eq!(FAILED_PAREN_PREFIX, format!("{MARKER_FAILED} ("));
        assert_eq!(FAILED_COLON_PREFIX, format!("{MARKER_FAILED}:"));
        let prefixes: Vec<&str> = FAILURE_VERDICTS.iter().map(|v| v.prefix).collect();
        assert_eq!(
            prefixes,
            vec![MARKER_REPLAN, FAILED_PAREN_PREFIX, FAILED_COLON_PREFIX],
            "table order is the UI's precedence order: replan, then `FAILED (`, then `FAILED:`"
        );
        assert_eq!(failure_verdicts(), FAILURE_VERDICTS);
        for verdict in FAILURE_VERDICTS {
            assert!(
                MARKER_LITERALS
                    .iter()
                    .any(|token| verdict.prefix.contains(token)),
                "failure prefix {:?} is not spelled from a marker constant",
                verdict.prefix
            );
        }
    }

    /// (d) Table-driven: the reason clause of `FAILED (…)`, `FAILED:` and
    /// `REPLAN REQUIRED` is extracted correctly, and the replan verdict wins
    /// over a `FAILED` shape in the same text.
    #[test]
    fn failure_reason_extraction_is_table_driven() {
        let cases: &[(&str, Option<&str>)] = &[
            // the exact abort trailer `failed_trailer("aborted")` emits
            (
                "Task aborted by user instruction.\n\nFAILED (aborted)",
                Some("aborted"),
            ),
            (
                "FAILED (Validator rejected deliverable)",
                Some("Validator rejected deliverable"),
            ),
            (
                "FAILED: compilation error on line 42",
                Some("compilation error on line 42"),
            ),
            (
                "REPLAN REQUIRED (t-002): task too complex — exceeded reasoning budget",
                Some("task too complex — exceeded reasoning budget"),
            ),
            (
                "REPLAN REQUIRED: missing dependency foo",
                Some("missing dependency foo"),
            ),
            (
                "REPLAN REQUIRED the plan is wrong",
                Some("the plan is wrong"),
            ),
            // nested parentheses: the reason ends at the first `)`
            ("notes (see below) FAILED (boom) tail", Some("boom")),
            // a multi-line reason keeps only its first non-blank line
            ("FAILED: first line\nsecond line", Some("first line")),
            ("FAILED: \n\n  wrapped reason\n", Some("wrapped reason")),
            (
                "FAILED (\n  multiline inside parens\n)",
                Some("multiline inside parens"),
            ),
            // a verdict shape with no reason clause yields None, so the caller
            // can fall back to its own heuristics
            ("FAILED ()", None),
            ("FAILED (", None),
            ("REPLAN REQUIRED (t-003)", None),
            ("REPLAN REQUIRED", None),
            ("", None),
            ("no verdict here at all", None),
            // case-sensitive by design: `failed:` is prose, not an emitted verdict
            ("failed: not a canonical shape", None),
            // replan outranks the FAILED shapes (table order)
            (
                "FAILED: stale note\nREPLAN REQUIRED: the decomposition is wrong",
                Some("the decomposition is wrong"),
            ),
            // non-marker cues are deliberately not this parser's business
            ("VALIDATOR REJECTION: no tools were run", None),
            ("ERROR: timeout waiting for response", None),
            ("Task aborted by user instruction.", None),
        ];
        for (input, expected) in cases {
            assert_eq!(failure_reason(input), *expected, "input: {input:?}");
        }
    }

    /// The emit side and the parse side are the same owner: whatever
    /// [`failed_trailer`] writes, [`failure_reason`] reads back.
    #[test]
    fn failed_trailer_round_trips_through_the_reason_parser() {
        for context in [
            "aborted",
            "Validator rejected deliverable",
            "task too complex",
        ] {
            let body = format!(
                "Task aborted by user instruction.\n\n{}",
                failed_trailer(context)
            );
            assert_eq!(failure_reason(&body), Some(context));
            assert!(has_failure_marker(&body));
        }
        for reason in ["the decomposition is wrong", "missing dependency foo"] {
            let body = format!("{MARKER_REPLAN}: {reason}");
            assert_eq!(failure_reason(&body), Some(reason));
            assert!(has_replan_marker(&body));
        }
    }

    /// (b, gate t-070) The **aborted-deliverable body** has exactly one owner.
    /// The exact bytes are pinned — several call sites assert them and
    /// `ui::helpers::extract_failure_reason` reads the trailer — and the body
    /// must read back through the same module that wrote it.
    #[test]
    fn aborted_deliverable_body_is_single_owned_and_parses_back() {
        // The historical wording of every one of the four former copies.
        assert_eq!(
            aborted_deliverable("aborted by user instruction"),
            "Task aborted by user instruction.\n\nFAILED (aborted)"
        );
        assert_eq!(
            aborted_deliverable("execution interrupted or runtime shutting down"),
            "Task execution interrupted or runtime shutting down.\n\nFAILED (aborted)"
        );
        assert_eq!(
            aborted_deliverable("execution thread interrupted or runtime shutting down"),
            "Task execution thread interrupted or runtime shutting down.\n\nFAILED (aborted)"
        );

        for context in [
            "aborted by user instruction",
            "execution interrupted or runtime shutting down",
        ] {
            let body = aborted_deliverable(context);
            // emit → parse: the trailer is the canonical `FAILED (<reason>)`
            // shape, and the marker reason is the shared [`ABORT_REASON`].
            assert_eq!(failure_reason(&body), Some(ABORT_REASON));
            assert!(has_failure_marker(&body));
            assert!(
                matches!(
                    MissionMarker::parse(&body),
                    Some(MissionMarker::Failed { .. })
                ),
                "an aborted deliverable must never parse as a success: {body:?}"
            );
            assert!(!has_complete_marker(&body));
            assert!(!has_replan_marker(&body));
        }
    }

    /// (b, gate t-070) Ownership guard for the four sites that used to
    /// hand-build nearly identical abort text: each must name the owner and
    /// none may re-type the body's sentence or trailer. Scanned **brace-aware**,
    /// because `src/orchestrator/mod.rs` hides production code behind an early
    /// `#[cfg(test)]` item — the very blind spot [`HAND_OFF_MARKER_SITES`] used
    /// to pin.
    #[test]
    fn abort_sites_call_the_body_owner_instead_of_building_it() {
        let manifest = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        for rel in [
            "src/orchestrator/mod.rs",
            "src/orchestrator/delegate.rs",
            "src/agents/mod.rs",
            "src/agents/runner/execution.rs",
        ] {
            let text = std::fs::read_to_string(manifest.join(rel)).expect("source readable");
            let code: String = production_lines_numbered(&text, true)
                .into_iter()
                .map(|(_, line)| line)
                .collect::<Vec<&str>>()
                .join("\n");

            assert!(
                code.contains("aborted_deliverable("),
                "{rel} no longer calls the single abort-body owner \
                 markers::aborted_deliverable"
            );
            for hand_built in [
                "Task aborted by user instruction",
                "Task {context}",
                "\\n\\nFAILED",
            ] {
                assert!(
                    !code.contains(hand_built),
                    "{rel} hand-builds the abort body again ({hand_built:?}) — call \
                     markers::aborted_deliverable instead"
                );
            }
        }
    }

    /// The verdict-word table is what `agents::validation` compares a whole
    /// verdict *field* against: exact, case-insensitive, no substring matching.
    #[test]
    fn failure_verdict_word_table_matches_the_validator_vocabulary() {
        for word in [
            "REJECTED",
            "rejected",
            "REJECT",
            "reject",
            "FAIL",
            "fail",
            "FAILED",
            "failed",
            "DECLINED",
            "DECLINE",
            "DISAPPROVED",
        ] {
            assert!(
                is_failure_verdict_word(word),
                "{word} must be a failure verdict"
            );
        }
        for not_a_verdict in [
            "",
            "APPROVED",
            "PASS",
            "OK",
            "SUCCESS",
            "FAILURE",
            "FAILED THE BUILD",
            "fail ",
            "unapproved",
        ] {
            assert!(
                !is_failure_verdict_word(not_a_verdict),
                "{not_a_verdict:?} must not be a failure verdict word"
            );
        }
        for word in FAILURE_VERDICT_WORDS {
            assert_eq!(
                *word,
                word.to_ascii_uppercase(),
                "{word} must be upper-case"
            );
        }
    }

    /// The prose-cue table is what `agents::validation` scans free-form
    /// comments with: substring matching against already-uppercased text.
    #[test]
    fn failure_prose_cues_match_uppercased_comment_text() {
        for (comment, expected) in [
            ("the build FAILED", true),
            ("build FAILURE", true),
            ("REJECTED the approach", true),
            ("rejected: no tests", true),
            ("looks good, ship it", false),
            ("", false),
            ("APPROVED", false),
        ] {
            assert_eq!(
                has_failure_verdict_cue(&comment.to_ascii_uppercase()),
                expected,
                "comment: {comment:?}"
            );
        }
        for cue in FAILURE_PROSE_CUES {
            assert_eq!(*cue, cue.to_ascii_uppercase(), "{cue} must be upper-case");
        }
    }

    /// The failure *stem* is the weakest predicate and is never a verdict:
    /// `failure` reads like a failure but is not the `FAILED` marker.
    #[test]
    fn failure_word_stem_is_weaker_than_the_marker() {
        for text in [
            "there was a failure",
            "FAILED",
            "FAILURE ahead",
            "Task failed to start",
            "Failing tests found",
        ] {
            assert!(has_failure_word(text), "{text:?} reads like a failure");
        }
        for text in ["all green", "Task completed", ""] {
            assert!(
                !has_failure_word(text),
                "{text:?} does not read like a failure"
            );
        }
        // The distinction is deliberate: only the marker predicate is a verdict.
        assert!(!has_failure_marker("FAILURE ahead"));
        assert!(has_failure_marker("FAILURE ahead FAILED"));
        for stem in FAILURE_WORD_STEMS {
            assert_eq!(
                *stem,
                stem.to_ascii_lowercase(),
                "{stem} must be lower-case"
            );
        }
    }

    // ---- gate t-030: single-owner guard tests -----------------------------

    /// Marker literals that may legitimately appear in this module: the
    /// constants themselves, their case-folded twins and the benign-counter
    /// table. Anywhere else in `src/` a marker literal is a re-implementation
    /// of this grammar.
    const MARKER_LITERALS: &[&str] = &[MARKER_COMPLETE, MARKER_REPLAN, MARKER_FAILED];

    /// Production files outside this module that are nevertheless allowed to
    /// spell a marker literal: `(file, justification-with-owner)` pairs. The
    /// escape hatch of the single-owner guard.
    ///
    /// Gate t-059 turned this from an archive of excuses into a **ratchet**. An
    /// entry is accepted only while all of the following hold, and any breach
    /// fails the guard (it used to be an `eprintln!` note):
    ///
    /// 1. the entry names a file that is not `src/markers.rs`;
    /// 2. it carries a documented justification of at least
    ///    [`MIN_JUSTIFICATION_LEN`] characters naming who owns it and why the
    ///    grammar cannot be called instead;
    /// 3. it is **live** — the named file really does still contain a marker
    ///    literal, so a file that has been migrated must be deleted from the
    ///    list instead of rotting in it.
    ///
    /// The list is EMPTY: the five entries gate t-030 needed are gone.
    /// `src/agents/mod.rs`, `src/agents/validation.rs`, `src/ui/helpers.rs` and
    /// `src/ui/raw.rs` now call the vocabulary API; `src/manager/phase.rs` was
    /// already clean (its entry had gone stale and the old guard only muttered
    /// about it).
    const KNOWN_EXTERNAL_MARKER_SITES: &[(&str, &str)] = &[];

    /// Shortest justification an escape-hatch entry may carry. A bare name or
    /// "TODO" is not a justification.
    const MIN_JUSTIFICATION_LEN: usize = 40;

    /// Hand-off inventory — **not** a vocabulary exemption, and now **EMPTY**.
    ///
    /// The primary guard scans line-by-line and stops at the first
    /// `#[cfg(test)]` attribute (see [`production_lines_numbered`]), so a file
    /// that places a `#[cfg(test)]` item *before* its production code hides part
    /// of that production code from the scan. `HAND_OFF_MARKER_SITES` pinned that
    /// blind spot: the brace-aware sweep below must never find a hidden marker
    /// literal in a file that is not listed here, i.e. the blind spot could never
    /// grow, and every entry named a file owned by somebody else that session.
    ///
    /// The list is EMPTY: gate t-068 routed the last entry
    /// (`src/orchestrator/mod.rs:315`, which hand-built `"…\n\nFAILED (aborted)"`)
    /// through the vocabulary API, and gate t-070 made that file — and every
    /// other abort site — call the single body owner
    /// [`aborted_deliverable`](crate::markers::aborted_deliverable). No
    /// production file outside this module spells a marker literal any more,
    /// hidden or otherwise, so the ratchet below has nothing left to tolerate:
    /// an empty inventory means the brace-aware sweep must find **zero** hidden
    /// marker literals, and a new one fails the guard outright. There is no
    /// allowlist and no note-taking escape hatch here.
    const HAND_OFF_MARKER_SITES: &[&str] = &[];

    /// Every `.rs` file under `src/`, recursively.
    fn rust_sources(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => panic!("cannot read {}: {e}", dir.display()),
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                rust_sources(&path, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                out.push(path);
            }
        }
    }

    /// `true` when the marker literal at `bytes[from..to]` is written *as* a
    /// marker. Only an underscore-joined identifier run disqualifies it, so the
    /// `FAILED` inside `MARKER_FAILED` / `FAILED_UPPER` is not counted, while the
    /// `FAILED` inside a `"…\n\nFAILED (aborted)"` string literal is — the `n` of
    /// the `\n` escape is an ordinary neighbour of the literal, not part of an
    /// identifier.
    fn is_marker_literal(bytes: &[u8], from: usize, to: usize) -> bool {
        let glued_before = from > 0 && bytes[from - 1] == b'_';
        let glued_after = to < bytes.len() && bytes[to] == b'_';
        !glued_before && !glued_after
    }

    /// `true` when `bytes[from..to]` is a whole identifier/word (used to find
    /// real references to the `MARKER_*` constants, ignoring e.g.
    /// `LEGACY_MARKER_COMPLETE`).
    fn is_identifier_occurrence(bytes: &[u8], from: usize, to: usize) -> bool {
        let glued = |i: usize| bytes[i].is_ascii_alphanumeric() || bytes[i] == b'_';
        !(from > 0 && glued(from - 1)) && !(to < bytes.len() && glued(to))
    }

    /// `(relative path, line, text)` for every marker literal written in
    /// production code: comment lines and everything from the first
    /// `#[cfg(test)]` attribute onwards (test scaffolding) are ignored.
    ///
    /// `brace_aware` selects how test scaffolding is recognised — see
    /// [`production_lines_numbered`] for the two modes and for why the primary
    /// guard still uses the conservative one.
    fn production_marker_libraries(brace_aware: bool) -> Vec<(String, usize, String)> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&root, &mut files);
        let mut sites = Vec::new();
        for path in files {
            let rel = path
                .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                .expect("path under manifest dir")
                .to_string_lossy()
                .replace('\\', "/");
            let name = path.file_name().expect("file name").to_string_lossy();
            if name.ends_with("_tests.rs") || name == "tests.rs" {
                continue;
            }
            let text = std::fs::read_to_string(&path).expect("utf-8 source");
            for (line_no, line) in production_lines_numbered(&text, brace_aware) {
                let bytes = line.as_bytes();
                for literal in MARKER_LITERALS {
                    let mut from = 0usize;
                    while let Some(offset) = line[from..].find(literal) {
                        let start = from + offset;
                        let end = start + literal.len();
                        if is_marker_literal(bytes, start, end) {
                            sites.push((rel.clone(), line_no, line.trim().to_string()));
                        }
                        from = end;
                    }
                }
            }
        }
        sites
    }

    /// The grammar must have exactly one owner: apart from `src/markers.rs`
    /// itself, no production file may contain a marker literal — and every
    /// escape-hatch entry must still be documented and live (ratchet, gate
    /// t-059).
    #[test]
    fn only_markers_module_spells_marker_literals_in_production_code() {
        let offenders = production_marker_libraries(false);
        let untracked: Vec<String> = offenders
            .iter()
            .filter(|(file, _, _)| {
                file != "src/markers.rs"
                    && !KNOWN_EXTERNAL_MARKER_SITES
                        .iter()
                        .any(|(entry, _)| entry == file)
            })
            .map(|(file, line, text)| format!("{file}:{line}: {text}"))
            .collect();
        assert!(
            untracked.is_empty(),
            "the mission-marker grammar is re-implemented outside src/markers.rs \
             (use crate::markers::has_*_marker / failure_reason / \
             is_failure_verdict_word / decorated / failed_trailer / \
             revoke_completion instead):\n{}",
            untracked.join("\n")
        );

        // Ratchet (gate t-059): a whitelist entry is only tolerated while it is
        // documented and still live. Stale or hand-wavy entries fail the build;
        // the old `eprintln!` note that let `src/manager/phase.rs` rot in the
        // list is gone.
        let live: std::collections::BTreeSet<&str> =
            offenders.iter().map(|(f, _, _)| f.as_str()).collect();
        let violations = whitelist_violations(KNOWN_EXTERNAL_MARKER_SITES, &live);
        assert!(
            violations.is_empty(),
            "KNOWN_EXTERNAL_MARKER_SITES is an escape hatch, not an archive:\n{}",
            violations.join("\n")
        );
    }

    /// The ratchet rule itself, as a pure function so it can be tested with
    /// synthetic entries (the real list is empty, so the guard alone would
    /// never exercise it).
    fn whitelist_violations(
        entries: &[(&str, &str)],
        live: &std::collections::BTreeSet<&str>,
    ) -> Vec<String> {
        entries
            .iter()
            .filter_map(|(file, justification)| {
                let mut why = Vec::new();
                if file.trim().is_empty() {
                    why.push("entry names no file".to_string());
                }
                if *file == "src/markers.rs" {
                    why.push("this module never needs an exemption".to_string());
                }
                if justification.trim().len() < MIN_JUSTIFICATION_LEN {
                    why.push(format!(
                        "justification is missing or shorter than {MIN_JUSTIFICATION_LEN} \
                         characters — name the owning task and why the marker API cannot be \
                         used"
                    ));
                }
                if !live.contains(file) {
                    why.push(
                        "the file no longer contains a marker literal — remove the entry \
                         (the whitelist must not archive finished work)"
                            .to_string(),
                    );
                }
                (!why.is_empty()).then(|| format!("{file}: {}", why.join("; ")))
            })
            .collect()
    }

    /// (b) The whitelist is EMPTY — and specifically does not archive any of
    /// the sites gate t-044 sweep A named, including the stale
    /// `src/manager/phase.rs` entry the old guard only printed a note about.
    #[test]
    fn external_marker_whitelist_is_empty() {
        let listed: Vec<&str> = KNOWN_EXTERNAL_MARKER_SITES
            .iter()
            .map(|(file, _)| *file)
            .collect();
        assert!(
            listed.is_empty(),
            "KNOWN_EXTERNAL_MARKER_SITES must be empty; every entry needs a live, \
             documented justification and the marker grammar has exactly one owner. \
             Currently listed: {listed:?}"
        );
        for retired in [
            "src/agents/mod.rs",
            "src/agents/validation.rs",
            "src/manager/phase.rs",
            "src/ui/helpers.rs",
            "src/ui/raw.rs",
        ] {
            assert!(
                !listed.contains(&retired),
                "{retired} was migrated (or was already clean) and must not be in \
                 KNOWN_EXTERNAL_MARKER_SITES"
            );
        }
    }

    /// The ratchet rule rejects undocumented, self-referential and stale
    /// entries, and accepts a documented live one — proved directly, so an
    /// empty real whitelist cannot hide a broken guard.
    #[test]
    fn whitelist_ratchet_rejects_undocumented_or_stale_entries() {
        let mut live = std::collections::BTreeSet::new();
        live.insert("src/agents/mod.rs");

        // Undocumented (empty / too short) -> violation.
        assert_eq!(
            whitelist_violations(&[("src/agents/mod.rs", "")], &live).len(),
            1,
            "an empty justification must not excuse a marker literal"
        );
        assert_eq!(
            whitelist_violations(&[("src/agents/mod.rs", "TODO")], &live).len(),
            1,
            "\"TODO\" is not a justification"
        );
        // Stale (the file no longer has a marker literal) -> violation.
        assert_eq!(
            whitelist_violations(
                &[(
                    "src/manager/phase.rs",
                    "owned by another task of the t-030 split; migrated later"
                )],
                &live
            )
            .len(),
            1,
            "a stale entry must fail the guard instead of printing a note"
        );
        // Pointing at the grammar owner itself -> violation.
        assert_eq!(
            whitelist_violations(
                &[(
                    "src/markers.rs",
                    "this module obviously owns the grammar already"
                )],
                &live
            )
            .len(),
            1
        );
        // Documented + live + external -> tolerated.
        assert!(
            whitelist_violations(
                &[(
                    "src/agents/mod.rs",
                    "t-999 keeps the wire format of a persisted record and may not \
                     touch the marker API"
                )],
                &live
            )
            .is_empty()
        );
        assert!(
            whitelist_violations(KNOWN_EXTERNAL_MARKER_SITES, &live).is_empty(),
            "the real whitelist must satisfy the ratchet"
        );
    }

    /// (a) Production code outside `src/markers.rs` contains **zero** marker
    /// literals — the whitelist can no longer absorb any of them.
    #[test]
    fn production_code_outside_markers_has_zero_marker_literals() {
        let external: Vec<String> = production_marker_libraries(false)
            .iter()
            .filter(|(file, _, _)| file != "src/markers.rs")
            .map(|(file, line, text)| format!("{file}:{line}: {text}"))
            .collect();
        assert_eq!(
            external,
            Vec::<String>::new(),
            "marker literals re-appeared outside the grammar owner:\n{}",
            external.join("\n")
        );
    }

    /// (a, targeted) Each site gate t-044 sweep A named is provably clean, so
    /// a regression in one of them is attributed to that file.
    #[test]
    fn migrated_sites_contain_no_marker_literals() {
        let offenders = production_marker_libraries(false);
        for file in [
            "src/agents/mod.rs",
            "src/agents/validation.rs",
            "src/manager/phase.rs",
            "src/ui/helpers.rs",
            "src/ui/raw.rs",
            "src/ui/transcript.rs",
        ] {
            let hits: Vec<String> = offenders
                .iter()
                .filter(|(f, _, _)| f == file)
                .map(|(_, line, text)| format!("{file}:{line}: {text}"))
                .collect();
            assert!(
                hits.is_empty(),
                "{file} hand-spells the marker vocabulary again (call the vocabulary \
                 API in src/markers.rs instead):\n{}",
                hits.join("\n")
            );
        }
    }

    /// Witness that the hand-off inventory is live: the brace-aware sweep sees
    /// production code that the conservative scan stops in front of, so a marker
    /// literal written after an early `#[cfg(test)]` item is still detected.
    #[test]
    fn brace_aware_sweep_sees_what_the_conservative_scan_hides() {
        let fixture = "#[cfg(test)]\nfn helper() { let _ = 1; }\n\n\
                       pub fn worker() -> String {\n    \"aborted\".to_string()\n}\n";
        let conservative: Vec<&str> = production_lines_numbered(fixture, false)
            .into_iter()
            .map(|(_, line)| line.trim())
            .collect();
        let strict: Vec<&str> = production_lines_numbered(fixture, true)
            .into_iter()
            .map(|(_, line)| line.trim())
            .collect();
        assert!(
            conservative.is_empty(),
            "conservative mode must stop at the attribute, saw {conservative:?}"
        );
        assert!(
            strict.contains(&"pub fn worker() -> String {")
                && strict.contains(&"\"aborted\".to_string()"),
            "brace-aware mode must keep scanning after the test item, saw {strict:?}"
        );
    }

    /// (a, fully owned) The brace-aware sweep — which sees the production code a
    /// file hides behind an early `#[cfg(test)]` item — must find **zero** marker
    /// literals outside `src/markers.rs`. With [`HAND_OFF_MARKER_SITES`] empty
    /// there is no declared hand-off left to absorb anything, so a marker literal
    /// written after a `#[cfg(test)]` item fails the guard just as loudly as one
    /// the conservative scan can see.
    #[test]
    fn marker_literals_hidden_by_the_conservative_scan_cannot_grow() {
        let strict = production_marker_libraries(true);
        let conservative: std::collections::BTreeSet<(String, usize)> =
            production_marker_libraries(false)
                .into_iter()
                .map(|(file, line, _)| (file, line))
                .collect();
        let hidden: Vec<String> = strict
            .iter()
            .filter(|(file, line, _)| {
                file != "src/markers.rs" && !conservative.contains(&(file.clone(), *line))
            })
            .map(|(file, line, text)| format!("{file}:{line}: {text}"))
            .collect();

        let unannounced: Vec<&String> = hidden
            .iter()
            .filter(|site| {
                !HAND_OFF_MARKER_SITES
                    .iter()
                    .any(|file| site.starts_with(&format!("{file}:")))
            })
            .collect();
        assert!(
            unannounced.is_empty(),
            "marker literals are hidden from the single-owner guard by a \
             `#[cfg(test)]` item placed before production code — migrate them to the \
             vocabulary API in src/markers.rs; HAND_OFF_MARKER_SITES is empty and stays \
             empty, so there is nothing left to hand off:\n{}",
            unannounced
                .iter()
                .map(|site| site.as_str())
                .collect::<Vec<&str>>()
                .join("\n")
        );

        // The inventory is EMPTY and must stay empty: a finished hand-off may
        // never be re-occupied, and every entry would be a marker literal the
        // grammar owner has not taken back yet.
        assert!(
            HAND_OFF_MARKER_SITES.is_empty(),
            "HAND_OFF_MARKER_SITES must be empty now that every production file \
             calls the vocabulary API; currently listed: {HAND_OFF_MARKER_SITES:?}"
        );
        for retired in ["src/orchestrator/mod.rs", "src/orchestrator/delegate.rs"] {
            assert!(
                !HAND_OFF_MARKER_SITES.contains(&retired),
                "{retired} was migrated (t-068 / t-070) and must not be back in \
                 HAND_OFF_MARKER_SITES"
            );
        }
        assert!(
            hidden.is_empty(),
            "the brace-aware sweep sees marker literals the conservative scan hides:\n{}",
            hidden.join("\n")
        );
    }

    /// The production lines of a source file with their 1-based line numbers.
    ///
    /// Two recognition modes for test scaffolding:
    ///
    /// * `brace_aware == false` (conservative, used by the primary guard):
    ///   everything from the first line that carries a `#[cfg(test)]` attribute
    ///   onwards is treated as test scaffolding. That is exact for the usual
    ///   file layout (production code, then one `#[cfg(test)] mod tests`) but
    ///   hides production code in files that put a `#[cfg(test)]` item earlier.
    ///   The hidden set is pinned by [`HAND_OFF_MARKER_SITES`], so the blind
    ///   spot cannot grow.
    /// * `brace_aware == true`: the `#[cfg(test)]` attribute and the item it
    ///   governs (tracked by brace depth) are skipped and the rest of the file
    ///   is scanned. Used by the hand-off inventory to find what the
    ///   conservative mode hides.
    ///
    /// Comment-only lines are never counted in either mode.
    fn production_lines_numbered(text: &str, brace_aware: bool) -> Vec<(usize, &str)> {
        let is_comment = |trimmed: &str| {
            trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
        };
        if !brace_aware {
            return text
                .lines()
                .enumerate()
                .take_while(|(_, line)| !line.trim().starts_with("#[cfg(test)]"))
                .filter(|(_, line)| !is_comment(line.trim()))
                .map(|(idx, line)| (idx + 1, line))
                .collect();
        }

        let mut out = Vec::new();
        let mut depth = 0usize;
        let mut skip_item: Option<usize> = None;
        for (idx, line) in text.lines().enumerate() {
            let trimmed = line.trim();
            let opens = line.matches('{').count();
            let closes = line.matches('}').count();
            if let Some(base) = skip_item {
                depth += opens;
                depth = depth.saturating_sub(closes);
                if depth <= base {
                    skip_item = None;
                }
                continue;
            }
            if trimmed.starts_with("#[cfg(test)]") {
                skip_item = Some(depth);
                depth += opens;
                depth = depth.saturating_sub(closes);
                continue;
            }
            depth += opens;
            depth = depth.saturating_sub(closes);
            if !is_comment(trimmed) {
                out.push((idx + 1, line));
            }
        }
        out
    }

    /// The production lines of a source file (no line numbers, conservative mode).
    fn production_lines(text: &str) -> Vec<&str> {
        production_lines_numbered(text, false)
            .into_iter()
            .map(|(_, line)| line)
            .collect()
    }

    /// `true` when `line` mentions `name` as a whole identifier.
    fn mentions(line: &str, name: &str) -> bool {
        let bytes = line.as_bytes();
        let mut from = 0usize;
        while let Some(offset) = line[from..].find(name) {
            let start = from + offset;
            if is_identifier_occurrence(bytes, start, start + name.len()) {
                return true;
            }
            from = start + name.len();
        }
        false
    }

    /// The public marker constants must have real production consumers —
    /// otherwise "single owner" is only a comment, and the constants can be
    /// deleted without anyone noticing (the exact loophole gate t-030 caught).
    #[test]
    fn every_marker_constant_has_a_production_consumer() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let mut files = Vec::new();
        rust_sources(&root, &mut files);
        for constant in ["MARKER_COMPLETE", "MARKER_FAILED", "MARKER_REPLAN"] {
            let consumers: Vec<String> = files
                .iter()
                .filter_map(|path| {
                    let rel = path
                        .strip_prefix(env!("CARGO_MANIFEST_DIR"))
                        .expect("path under manifest dir")
                        .to_string_lossy()
                        .replace('\\', "/");
                    if rel == "src/markers.rs" {
                        return None;
                    }
                    let text = std::fs::read_to_string(path).expect("utf-8 source");
                    let hits = production_lines(&text)
                        .iter()
                        .filter(|line| mentions(line, constant))
                        .count();
                    (hits > 0).then(|| format!("{rel} ({hits})"))
                })
                .collect();
            assert!(
                !consumers.is_empty(),
                "{constant} has no production consumer outside src/markers.rs — \
                 the marker grammar is being re-implemented again"
            );
        }
    }
}
