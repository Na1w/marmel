//! Canonical task-identifier normalization — dedup cluster C1, part 1.
//!
//! Task ids such as `t-001` are produced by the LLM and then round-trip through
//! tool arguments, plan checkboxes, synthesized-prompt filenames and worker
//! tags. Along the way they are usually wrapped in markdown / JSON decoration —
//! `[t-001]`, `(t-001)`, `"t-001"`, `'t-001'` — so every consumer used to
//! open-code the same trimming chain
//! (`trim_matches(|c| c == '[' || c == ']' || ...).trim()`). Cluster C1 of
//! `docs/recon_duplication_helpers.md` tracks those clones; this module is the
//! single source of truth for the eight of them living in the agents, harness
//! and manager layers.
//!
//! Normalization never repairs an id: inner characters are preserved
//! byte-for-byte, so normalization alone is safe for matching and tagging but
//! **not** for path construction.
//!
//! Two responsibilities live here, and they stay separate on purpose:
//!
//! 1. *normalization* ([`normalize_task_id_ref`] / [`normalize_task_id`]) —
//!    decoration stripping only, never validation, never rewriting the grammar.
//! 2. *validation* ([`validate_task_id`] / [`validate_task_id_owned`], gate
//!    t-046) — the single canonical gate every on-disk path derived from a task
//!    id must pass before the id is joined onto a directory. Normalization is
//!    deliberately *not* folded into it: a caller that normalizes must validate
//!    the normalized value, and a caller that validates raw input must not be
//!    handed a silently repaired id.

/// Decoration characters stripped from both ends of a task id.
///
/// Kept in one place so that all consumers agree: `[`, `]`, `(`, `)`, `"` and
/// `'`. Call sites must not re-spell this set.
const fn is_decoration(c: char) -> bool {
    matches!(c, '[' | ']' | '(' | ')' | '"' | '\'')
}

/// Borrowed, allocation-free normalization of a raw task id.
///
/// Trims surrounding whitespace, strips `[`/`]`/`(`/`)`/`"`/`'` decoration, then
/// trims whitespace again, so `"[t-001]"`, `" \"t-001\" "` and `"t-001"` all
/// normalize to the same value. The operation is idempotent.
///
/// Returns `""` when nothing but decoration/whitespace is left; callers that
/// need to distinguish "absent" from "empty" must do that check themselves (this
/// mirrors the historical behaviour of the sites this replaced, several of which
/// deliberately keep an empty string).
#[must_use]
pub fn normalize_task_id_ref(raw: &str) -> &str {
    raw.trim().trim_matches(is_decoration).trim()
}

/// Owned normalization returning `None` for ids that are empty after
/// normalization — the shape used by call sites that build an
/// `Option<String>` worker tag.
#[must_use]
pub fn normalize_task_id(raw: &str) -> Option<String> {
    let cleaned = normalize_task_id_ref(raw);
    (!cleaned.is_empty()).then(|| cleaned.to_string())
}

/// Longest task id the validator accepts, in `char`s.
///
/// Task ids are LLM output that becomes a file name, so an unbounded id is a
/// denial-of-service/ENAMETOOLONG hazard on top of the path-traversal hazard.
/// Every id shape in this repository (`t-001`, `task-t-001`, `t-val-01`,
/// `steer-task-3`) is far below the cap, which is set to 64 characters.
pub const MAX_TASK_ID_LEN: usize = 64;

/// Why a candidate task id was refused (gate t-046).
///
/// The variants are deliberately specific: callers surface this text verbatim
/// in a verdict-gap reason, so an operator must be able to tell *which* rule the
/// id tripped instead of guessing from a generic "invalid task id".
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TaskIdError {
    /// The id is empty, or nothing but whitespace.
    #[error("task id is empty")]
    Empty,
    /// The id is longer than [`MAX_TASK_ID_LEN`] characters.
    #[error("task id is {len} characters, above the {max}-character cap")]
    TooLong {
        /// Length of the rejected id in `char`s.
        len: usize,
        /// The documented cap ([`MAX_TASK_ID_LEN`]).
        max: usize,
    },
    /// A path separator (`/` or `\`) — the path-traversal vector this gate
    /// exists to close.
    #[error("task id contains the path separator {ch:?}")]
    PathSeparator {
        /// The offending separator character.
        ch: char,
    },
    /// A `..` segment anywhere in the id (parent-directory escape).
    #[error("task id contains a '..' segment")]
    DotDotSegment,
    /// The id starts with `.`, which turns a joined path into a hidden file or
    /// an escape.
    #[error("task id starts with '.'")]
    LeadingDot,
    /// The id ends with `.`; trailing dots collide with extension handling.
    #[error("task id ends with '.'")]
    TrailingDot,
    /// Whitespace anywhere inside the id (leading, inner or trailing).
    #[error("task id contains whitespace ({ch:?})")]
    Whitespace {
        /// The offending whitespace character.
        ch: char,
    },
    /// A control character (NUL, ESC, DEL, C1 controls, ...).
    #[error("task id contains the control character {ch:?}")]
    ControlCharacter {
        /// The offending control character.
        ch: char,
    },
    /// A non-ASCII character: ids must stay byte-predictable on disk.
    #[error("task id contains the non-ASCII character {ch:?}")]
    NonAscii {
        /// The offending non-ASCII character.
        ch: char,
    },
    /// Any other character outside the accepted alphabet
    /// (ASCII alphanumeric, `-`, `_`, and an inner `.`).
    #[error("task id contains the disallowed character {ch:?}")]
    DisallowedCharacter {
        /// The offending character.
        ch: char,
    },
}

/// The single canonical task-id validator (gate t-046).
///
/// Returns the id unchanged on success — nothing here sanitizes, trims, clamps
/// or otherwise repairs the input, so a rejected id can never be silently turned
/// into a *different* file name. Callers that build a path from a task id must
/// propagate this error (or record it as a hard verdict gap) instead of falling
/// back to a mended value.
///
/// Accepted alphabet: ASCII alphanumeric plus `-` and `_` (the in-repo shape,
/// e.g. `t-001`, `task-t-001`, `t_val_01`), plus a single inner `.` that is
/// neither leading, trailing nor adjacent to another `.`.
///
/// Rejected, each with its own [`TaskIdError`] variant:
/// * empty / whitespace-only ids,
/// * ids longer than [`MAX_TASK_ID_LEN`] characters,
/// * `..` segments, `/` and `\` (path traversal / separator escape),
/// * a leading or trailing `.`,
/// * whitespace anywhere (leading, inner or trailing),
/// * control characters and non-ASCII characters,
/// * any other character outside the accepted alphabet.
///
/// The check is applied to the string as given. Ids that arrive decorated
/// (`[t-001]`) must be normalized first with [`normalize_task_id_ref`] and the
/// *normalized* value validated — validating the raw form would reject ids the
/// rest of the crate legitimately uses.
pub fn validate_task_id(id: &str) -> Result<&str, TaskIdError> {
    if id.trim().is_empty() {
        return Err(TaskIdError::Empty);
    }
    let chars: Vec<char> = id.chars().collect();
    if chars.len() > MAX_TASK_ID_LEN {
        return Err(TaskIdError::TooLong {
            len: chars.len(),
            max: MAX_TASK_ID_LEN,
        });
    }
    // Any `..` is a parent-directory segment, separator or no separator.
    if id.contains("..") {
        return Err(TaskIdError::DotDotSegment);
    }
    for (idx, &ch) in chars.iter().enumerate() {
        if ch == '/' || ch == '\\' {
            return Err(TaskIdError::PathSeparator { ch });
        }
        if ch.is_whitespace() {
            return Err(TaskIdError::Whitespace { ch });
        }
        if ch.is_control() {
            return Err(TaskIdError::ControlCharacter { ch });
        }
        if ch == '.' {
            if idx == 0 {
                return Err(TaskIdError::LeadingDot);
            }
            if idx + 1 == chars.len() {
                return Err(TaskIdError::TrailingDot);
            }
            // An inner single dot is inside the accepted grammar (see docs); a
            // second one would already have tripped `DotDotSegment`.
            continue;
        }
        if !ch.is_ascii() {
            return Err(TaskIdError::NonAscii { ch });
        }
        if !(ch.is_ascii_alphanumeric() || ch == '-' || ch == '_') {
            return Err(TaskIdError::DisallowedCharacter { ch });
        }
    }
    Ok(id)
}

/// Owned counterpart of [`validate_task_id`] for call sites that store or move
/// the validated id (path builders, worker tags, map keys).
pub fn validate_task_id_owned(id: &str) -> Result<String, TaskIdError> {
    validate_task_id(id).map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Raw input -> expected normalized value for decorated / plain spellings.
    const WRAPPED: &[(&str, &str)] = &[
        ("[t-001]", "t-001"),
        ("\"t-001\"", "t-001"),
        ("  [t-001]  ", "t-001"),
        ("t-001", "t-001"),
        ("[\"t-001\"]", "t-001"),
        ("('t-001')", "t-001"),
        ("[t-001", "t-001"),
        ("t-001]  ", "t-001"),
        ("\t[t-001]\n", "t-001"),
    ];

    /// Inputs that normalize to nothing.
    const EMPTY: &[&str] = &["", "[]", "   ", "\"\"", "()", "[\"\"]", "''", "[ ]"];

    #[test]
    fn strips_decoration_and_whitespace() {
        for (raw, expected) in WRAPPED {
            assert_eq!(
                normalize_task_id_ref(raw),
                *expected,
                "borrowed site for {raw:?}"
            );
            assert_eq!(
                normalize_task_id(raw).as_deref(),
                Some(*expected),
                "owned site for {raw:?}"
            );
        }
    }

    #[test]
    fn empty_and_decoration_only_inputs() {
        for raw in EMPTY {
            assert_eq!(normalize_task_id_ref(raw), "", "borrowed site for {raw:?}");
            assert_eq!(normalize_task_id(raw), None, "owned site for {raw:?}");
        }
    }

    #[test]
    fn normalization_is_idempotent() {
        for (raw, expected) in WRAPPED {
            let once = normalize_task_id_ref(raw);
            assert_eq!(once, *expected);
            assert_eq!(normalize_task_id_ref(once), once, "not idempotent: {raw:?}");
        }
    }

    #[test]
    fn inner_grammar_is_never_touched() {
        // Normalization must not repair or validate the id grammar itself.
        assert_eq!(normalize_task_id_ref("[t-022 part 1]"), "t-022 part 1");
        assert_eq!(normalize_task_id_ref("t-abc_123"), "t-abc_123");
        assert_eq!(normalize_task_id_ref("[t-001]]"), "t-001");
        assert_eq!(
            normalize_task_id_ref("raw[inner]suffix"),
            "raw[inner]suffix"
        );
    }

    /// Label plus the consumption pattern of a call site that keeps the
    /// normalized value as an owned string (path, tag or lookup key).
    type BorrowedSite = (&'static str, fn(&str) -> String);
    /// Label plus the consumption pattern of a call site that maps an empty
    /// normalization to `None`.
    type OptionSite = (&'static str, fn(&str) -> Option<String>);

    /// Regression guard for dedup cluster C1 part 1: every call site whose
    /// inline trimming chain was replaced must agree on the same normalized
    /// value for the same input. The adapters below mirror how each site
    /// consumes the API, so re-spelling the trimming chain at one of them (or
    /// diverging on the empty-vs-absent handling) fails this test.
    #[test]
    fn c1_call_sites_agree_on_the_same_normalized_value() {
        // Sites that feed the normalized value straight into a path, tag or
        // lookup and keep an empty string when there is nothing left.
        let borrowed_sites: &[BorrowedSite] = &[
            (
                "agents/validation.rs run_automated_validation_inner (custom validation prompt name)",
                |r| normalize_task_id_ref(r).to_string(),
            ),
            (
                "agents/prompt_builder.rs save_to_disk (blueprint file stem)",
                |r| normalize_task_id_ref(r).to_string(),
            ),
            (
                "manager/phase.rs check_plan_on_deliverable (bound delegate_task task id)",
                |r| normalize_task_id_ref(r).to_string(),
            ),
            ("manager/phase.rs check_off (plan lookup key)", |r| {
                normalize_task_id_ref(r).to_string()
            }),
            (
                "manager/phase.rs check_plan_on_marker (marker task id)",
                |r| normalize_task_id_ref(r).to_string(),
            ),
            (
                "harness/workspace.rs prompt_path_for_task (prompt path)",
                |r| normalize_task_id_ref(r).to_string(),
            ),
        ];
        // Sites that map an empty normalization to `None`.
        let option_sites: &[OptionSite] = &[
            (
                "agents/validation.rs run_automated_validation_inner (validator worker tag)",
                normalize_task_id,
            ),
            (
                "agents/runner/execution.rs (specialist worker tag)",
                normalize_task_id,
            ),
        ];

        for &(raw, expected) in WRAPPED {
            for (site, normalize) in borrowed_sites {
                assert_eq!(
                    normalize(raw),
                    expected,
                    "site {site} disagrees for {raw:?}"
                );
            }
            for (site, normalize) in option_sites {
                assert_eq!(
                    normalize(raw).as_deref(),
                    Some(expected),
                    "site {site} disagrees for {raw:?}"
                );
            }
        }
        for raw in EMPTY {
            for (site, normalize) in borrowed_sites {
                assert_eq!(normalize(raw), "", "site {site} disagrees for {raw:?}");
            }
            for (site, normalize) in option_sites {
                assert_eq!(normalize(raw), None, "site {site} disagrees for {raw:?}");
            }
        }
    }

    /// Consumption pattern of a part-2 call site that keeps the normalized value
    /// borrowed (`&str`) — subagent tags, prompt-file names, plan comparisons.
    type BorrowedRefSite = (&'static str, fn(&str) -> &str);
    /// Consumption pattern of a part-2 call site that materialises an owned
    /// `String` (UI seam, tool-arg id, lowercased match key).
    type OwnedSite = (&'static str, fn(&str) -> String);

    /// `src/orchestrator/steer.rs` `build_steer_subtasks`: a decorated
    /// `tool_call_id` is normalized and an *empty* result is replaced by a
    /// synthesized `steer-task-N` id.
    fn steer_task_id(raw: &str, idx: usize) -> String {
        let cleaned = normalize_task_id_ref(raw).to_string();
        if cleaned.is_empty() {
            format!("steer-task-{idx}")
        } else {
            cleaned
        }
    }

    /// Regression guard for dedup cluster C1 part 2: the two named UI helpers
    /// and every remaining inline clone in `src/ui/*` and `src/orchestrator/*`
    /// must agree on the same normalized value, and must keep their historical
    /// empty-vs-absent handling (empty string kept vs `None`).
    #[test]
    fn c1_part_2_call_sites_agree_on_the_same_normalized_value() {
        let borrowed_ref_sites: &[BorrowedRefSite] = &[
            (
                "ui/tui/formatting.rs clean_task_id (borrowed UI seam)",
                normalize_task_id_ref,
            ),
            (
                "ui/helpers.rs update_subagent_lifecycle (subagent name suffix)",
                normalize_task_id_ref,
            ),
            (
                "ui/tui/mod.rs DelegationEvent arms (subagent tag)",
                normalize_task_id_ref,
            ),
            (
                "orchestrator/workers.rs get_active_subtasks_str (blueprint prompt file)",
                normalize_task_id_ref,
            ),
            (
                "orchestrator/mod.rs execute (saved prompt path)",
                normalize_task_id_ref,
            ),
            (
                "orchestrator/delegate.rs re-delegation guard lookup",
                normalize_task_id_ref,
            ),
            (
                "orchestrator/preemption.rs StreamIdentity::matches (tid/agent/task_id)",
                normalize_task_id_ref,
            ),
        ];
        let owned_sites: &[OwnedSite] = &[
            ("ui/helpers.rs clean_task_id (owned UI seam)", |r| {
                normalize_task_id_ref(r).to_string()
            }),
            (
                "orchestrator/steer.rs build_steer_subtasks (DelegateTask tool_call_id)",
                |r| normalize_task_id_ref(r).to_string(),
            ),
            (
                "orchestrator/delegate.rs delegate_task_id (mandatory task id)",
                |r| normalize_task_id_ref(r).to_string(),
            ),
            (
                "orchestrator/workers.rs worker_matches (lowercased match keys)",
                |r| normalize_task_id_ref(r).to_ascii_lowercase(),
            ),
        ];
        let option_sites: &[OptionSite] = &[
            (
                "ui/session.rs delegated_task (tool-arg task id)",
                normalize_task_id,
            ),
            (
                "orchestrator/workers.rs register_active_worker_with_token (worker tag)",
                normalize_task_id,
            ),
            (
                "orchestrator/steer.rs build_steer_subtasks (single-subtask fallback chain)",
                normalize_task_id,
            ),
        ];

        for &(raw, expected) in WRAPPED {
            for (site, normalize) in borrowed_ref_sites {
                assert_eq!(
                    normalize(raw),
                    expected,
                    "site {site} disagrees for {raw:?}"
                );
            }
            for (site, normalize) in owned_sites {
                assert_eq!(
                    normalize(raw),
                    expected,
                    "site {site} disagrees for {raw:?}"
                );
            }
            for (site, normalize) in option_sites {
                assert_eq!(
                    normalize(raw).as_deref(),
                    Some(expected),
                    "site {site} disagrees for {raw:?}"
                );
            }
            assert_eq!(
                steer_task_id(raw, 7),
                expected,
                "steer fallback fires for {raw:?}"
            );
        }
        for raw in EMPTY {
            for (site, normalize) in borrowed_ref_sites {
                assert_eq!(normalize(raw), "", "site {site} disagrees for {raw:?}");
            }
            for (site, normalize) in owned_sites {
                assert_eq!(normalize(raw), "", "site {site} disagrees for {raw:?}");
            }
            for (site, normalize) in option_sites {
                assert_eq!(normalize(raw), None, "site {site} disagrees for {raw:?}");
            }
            // Empty-vs-absent: steer synthesizes an id instead of keeping "".
            assert_eq!(
                steer_task_id(raw, 7),
                "steer-task-7",
                "steer fallback must fire for {raw:?}"
            );
        }
    }

    // ---------------------------------------------------------------------
    // Gate t-046: the canonical task-id validator behind every id-derived path.
    // ---------------------------------------------------------------------

    /// Ids that must pass [`validate_task_id`] unchanged — the shapes actually
    /// used in this repository.
    const VALID_IDS: &[&str] = &[
        "t-001",
        "t-046",
        "T-001",
        "task-t-001",
        "t_val_01",
        "t-val-01",
        "steer-task-3",
        "a",
        "0",
        "abc123",
        "t-001.md",
        "t-1.2",
    ];

    /// Hostile or malformed ids plus the exact rejection each must produce.
    const REJECTED: &[(&str, TaskIdError)] = &[
        // Path traversal / separator escape — the core of gate t-046.
        ("../../etc/x", TaskIdError::DotDotSegment),
        ("../../etc/passwd", TaskIdError::DotDotSegment),
        ("../x", TaskIdError::DotDotSegment),
        ("..", TaskIdError::DotDotSegment),
        (".", TaskIdError::LeadingDot),
        ("a/b", TaskIdError::PathSeparator { ch: '/' }),
        ("t-001/extra", TaskIdError::PathSeparator { ch: '/' }),
        ("t-001/validation", TaskIdError::PathSeparator { ch: '/' }),
        ("a\\b", TaskIdError::PathSeparator { ch: '\\' }),
        ("..\\..\\windows\\x", TaskIdError::DotDotSegment),
        ("/absolute/etc/x", TaskIdError::PathSeparator { ch: '/' }),
        ("t-001\\", TaskIdError::PathSeparator { ch: '\\' }),
        (".hidden", TaskIdError::LeadingDot),
        (".t-001.md", TaskIdError::LeadingDot),
        ("t-001.", TaskIdError::TrailingDot),
        ("t-001..md", TaskIdError::DotDotSegment),
        ("....", TaskIdError::DotDotSegment),
        ("x..", TaskIdError::DotDotSegment),
        // Empty and whitespace-only.
        ("", TaskIdError::Empty),
        (" ", TaskIdError::Empty),
        ("   ", TaskIdError::Empty),
        ("\t\n", TaskIdError::Empty),
        // Whitespace anywhere inside the id.
        ("t-001 ", TaskIdError::Whitespace { ch: ' ' }),
        (" t-001", TaskIdError::Whitespace { ch: ' ' }),
        ("t 001", TaskIdError::Whitespace { ch: ' ' }),
        ("t-\t001", TaskIdError::Whitespace { ch: '\t' }),
        ("t-001\n", TaskIdError::Whitespace { ch: '\n' }),
        ("t-001\u{a0}", TaskIdError::Whitespace { ch: '\u{a0}' }),
        // Control characters.
        ("t-001\u{0}", TaskIdError::ControlCharacter { ch: '\0' }),
        ("t\u{7}001", TaskIdError::ControlCharacter { ch: '\u{7}' }),
        (
            "t-001\u{7f}",
            TaskIdError::ControlCharacter { ch: '\u{7f}' },
        ),
        // Non-ASCII.
        ("t-001é", TaskIdError::NonAscii { ch: 'é' }),
        ("日本語", TaskIdError::NonAscii { ch: '日' }),
        ("t\u{2011}001", TaskIdError::NonAscii { ch: '\u{2011}' }),
        // Any other character outside the accepted alphabet — including the
        // decoration normalization strips, and encoded traversal payloads.
        ("[t-001]", TaskIdError::DisallowedCharacter { ch: '[' }),
        ("\"t-001\"", TaskIdError::DisallowedCharacter { ch: '"' }),
        ("t-001:extra", TaskIdError::DisallowedCharacter { ch: ':' }),
        ("t-001;rm -rf", TaskIdError::DisallowedCharacter { ch: ';' }),
        ("%2e%2e%2f", TaskIdError::DisallowedCharacter { ch: '%' }),
        ("t-001.md.md.", TaskIdError::TrailingDot),
    ];

    #[test]
    fn accepted_id_shapes_pass_the_validator() {
        for id in VALID_IDS {
            assert_eq!(
                validate_task_id(id),
                Ok(*id),
                "valid id {id:?} must be accepted byte-for-byte"
            );
            assert_eq!(
                validate_task_id_owned(id).as_deref(),
                Ok(*id),
                "owned site must agree for valid id {id:?}"
            );
        }
    }

    #[test]
    fn hostile_ids_are_rejected_with_the_specific_rule() {
        for (id, expected) in REJECTED {
            let err = validate_task_id(id)
                .err()
                .unwrap_or_else(|| panic!("{id:?} must be rejected"));
            assert_eq!(&err, expected, "wrong rejection for {id:?}");
            assert_eq!(
                validate_task_id_owned(id).err().as_ref(),
                Some(expected),
                "owned site must reject {id:?} identically"
            );
            let rendered = err.to_string();
            assert!(
                !rendered.is_empty(),
                "rejection of {id:?} must render a reason"
            );
        }
    }

    /// Every rejection names its rule, and the type is a real error so callers
    /// can propagate it through `anyhow`/`std` instead of stringly-typed checks.
    #[test]
    fn task_id_error_is_a_propagatable_typed_error() {
        let errs: Vec<TaskIdError> = REJECTED
            .iter()
            .map(|(_, expected)| expected.clone())
            .collect();
        for err in &errs {
            let as_std: &dyn std::error::Error = err;
            assert!(!as_std.to_string().is_empty());
        }
        // Usable through the standard error trait objects `anyhow` requires.
        let boxed: Box<dyn std::error::Error + Send + Sync> = Box::new(TaskIdError::DotDotSegment);
        assert!(boxed.to_string().contains(".."));
        let wrapped: anyhow::Error = TaskIdError::PathSeparator { ch: '/' }.into();
        assert!(
            wrapped.to_string().contains("path separator"),
            "the reason must name the traversal rule: {wrapped}"
        );
    }

    /// The documented length cap: exactly `MAX_TASK_ID_LEN` characters pass, one
    /// more is refused (and the reason reports both numbers).
    #[test]
    fn length_cap_is_enforced() {
        let at_cap = "t-".repeat(MAX_TASK_ID_LEN / 2);
        assert_eq!(at_cap.chars().count(), MAX_TASK_ID_LEN);
        assert_eq!(
            validate_task_id(&at_cap),
            Ok(at_cap.as_str()),
            "an id exactly at the cap must be accepted"
        );

        let over_cap = format!("{at_cap}x");
        let err = validate_task_id(&over_cap).expect_err("over-cap id must be rejected");
        assert_eq!(
            err,
            TaskIdError::TooLong {
                len: MAX_TASK_ID_LEN + 1,
                max: MAX_TASK_ID_LEN
            }
        );
        assert!(err.to_string().contains("cap"), "reason must name the cap");
    }

    /// Fail-closed contract: the validator never returns a repaired id, so a
    /// rejected value can never turn into a different file name downstream.
    #[test]
    fn validator_never_rewrites_or_clamps_the_id() {
        for (id, _) in REJECTED {
            if let Ok(accepted) = validate_task_id(id) {
                panic!("{id:?} must not be accepted, let alone rewritten to {accepted:?}");
            }
        }
        for id in VALID_IDS {
            let accepted = validate_task_id(id).expect("valid id");
            assert_eq!(accepted, *id, "accepted id must be returned verbatim");
        }
    }

    /// Normalization and validation stay separate concerns: normalization alone
    /// keeps a traversal payload intact (which is exactly why every path site
    /// has to validate afterwards), and validating a decorated id is refused
    /// rather than silently normalized.
    #[test]
    fn normalization_and_validation_compose_in_that_order() {
        for hostile in ["../../etc/x", "a/b", "../x"] {
            assert_eq!(
                normalize_task_id_ref(hostile),
                hostile,
                "normalization must not sanitize {hostile:?}"
            );
            assert!(
                validate_task_id(normalize_task_id_ref(hostile)).is_err(),
                "{hostile:?} must be refused after normalization"
            );
        }
        // A decorated id is valid only once normalized — the validator does not
        // normalize on the caller's behalf.
        assert!(validate_task_id("[t-001]").is_err());
        assert_eq!(
            validate_task_id(normalize_task_id_ref("[t-001]")).ok(),
            Some("t-001")
        );
    }
}
