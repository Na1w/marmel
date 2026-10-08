//! Named constants for the wire-visible tool names used across the codebase.
//!
//! Centralizing tool names as `pub const` strings prevents typos and keeps the
//! canonical set of tool names in a single, auditable location. Every module
//! that matches on, dispatches, or documents a tool name should reference these
//! constants rather than hardcoding the string literal, so the values stay in
//! lockstep (e.g. the harness dispatcher, the specialist `tool_namespaces`
//! allowlists, and the registry all agree).

/// `delegate_task` — dispatch a bounded unit of domain work to a specialist
/// subagent (REQ-ORCH-005).
pub const TOOL_DELEGATE_TASK: &str = "delegate_task";
/// `read_file` — read paginated lines of UTF-8 text from a file.
pub const TOOL_READ_FILE: &str = "read_file";
/// `write_file` — create a new file or completely overwrite an existing file.
pub const TOOL_WRITE_FILE: &str = "write_file";
/// `replace` — replace an exact, unique block of text within a file.
pub const TOOL_REPLACE: &str = "replace";
/// `run_command` — execute a command line inside a dedicated PTY.
pub const TOOL_RUN_COMMAND: &str = "run_command";
/// `grep_search` — search for a regex pattern across workspace files.
pub const TOOL_GREP_SEARCH: &str = "grep_search";
/// `glob` — find files matching a glob pattern.
pub const TOOL_GLOB: &str = "glob";
/// `create_plan` — write or overwrite the workspace execution plan.
pub const TOOL_CREATE_PLAN: &str = "create_plan";
/// `archive_current_plan` — archive the current execution plan to `.marmel/archive/`.
pub const TOOL_ARCHIVE_PLAN: &str = "archive_current_plan";
/// `rebirth` — compact conversation history into a structured checkpoint summary.
pub const TOOL_REBIRTH: &str = "rebirth";
/// `pty_spawn` — spawn an interactive persistent PTY terminal session.
pub const TOOL_PTY_SPAWN: &str = "pty_spawn";
/// `pty_write` — send input text or commands to an active PTY session.
pub const TOOL_PTY_WRITE: &str = "pty_write";
/// `pty_read` — read unread buffer output from an active PTY session.
pub const TOOL_PTY_READ: &str = "pty_read";
/// `pty_close` — close an active PTY session and kill its process group.
pub const TOOL_PTY_CLOSE: &str = "pty_close";
/// `pty_list` — list all active interactive PTY sessions.
pub const TOOL_PTY_LIST: &str = "pty_list";
/// `leave_verdict` — submit a formal validation verdict (APPROVED / REJECTED) with comments.
pub const TOOL_LEAVE_VERDICT: &str = "leave_verdict";
/// `sleep` — pause execution for a specified duration in seconds.
pub const TOOL_SLEEP: &str = "sleep";
/// `reply_to_arbitrator` — send a response or clarification back to the Steer Arbitrator.
pub const TOOL_REPLY_TO_ARBITRATOR: &str = "reply_to_arbitrator";

/// `terminal__read_file` — caesar-style namespaced variant of `read_file`.
pub const TERMINAL_READ_FILE: &str = "terminal__read_file";
/// `terminal__write_file` — caesar-style namespaced variant of `write_file`.
pub const TERMINAL_WRITE_FILE: &str = "terminal__write_file";
/// `terminal__replace` — caesar-style namespaced variant of `replace`.
pub const TERMINAL_REPLACE: &str = "terminal__replace";
/// `terminal__run_command` — caesar-style namespaced variant of `run_command`.
pub const TERMINAL_RUN_COMMAND: &str = "terminal__run_command";
/// `terminal__grep_search` — caesar-style namespaced variant of `grep_search`.
pub const TERMINAL_GREP_SEARCH: &str = "terminal__grep_search";
/// `terminal__glob` — caesar-style namespaced variant of `glob`.
pub const TERMINAL_GLOB: &str = "terminal__glob";
// `list_directory` / `terminal__list_directory` (t-069 decision, see the note on
// [`TOOL_ALIAS_TABLE`]): **no built-in handler implements these names.** They are
// deliberately *not* members of [`CANONICAL_TOOL_NAMES`] nor rows of
// [`TOOL_ALIAS_TABLE`], so the dispatcher claims neither and an MCP server is free
// to serve them. The constants exist only so the specialist blueprint lists that
// still name them (`agents::runner::fix_loop::INSPECTION_TOOLS`) keep compiling;
// a call with either spelling ends in [`crate::harness::ToolError::UnknownTool`].
/// `terminal__list_directory` — caesar-style namespaced variant of `list_directory`
/// (no built-in handler; see the note above).
pub const TERMINAL_LIST_DIRECTORY: &str = "terminal__list_directory";
/// `list_directory` — list the contents of a directory (no built-in handler; see
/// the note above).
pub const TOOL_LIST_DIRECTORY: &str = "list_directory";
/// `terminal__sleep` — caesar-style namespaced variant of `sleep`.
pub const TERMINAL_SLEEP: &str = "terminal__sleep";
/// `terminal__leave_verdict` — caesar-style namespaced variant of `leave_verdict`.
pub const TERMINAL_LEAVE_VERDICT: &str = "terminal__leave_verdict";

/// Namespace prefix for the caesar-style namespaced tool variants
/// (e.g. `terminal__read_file`). Use `terminal_tool()` to build a full
/// namespaced name from a bare tool name.
pub const TERMINAL_PREFIX: &str = "terminal__";

/// Build a caesar-style namespaced tool name from a bare tool name,
/// e.g. `terminal_tool(TOOL_READ_FILE)` == `"terminal__read_file"`.
pub fn terminal_tool(bare: &str) -> String {
    format!("{TERMINAL_PREFIX}{bare}")
}

// ── the one canonical tool-alias vocabulary (dedup gate t-044, sweep B) ──────
//
// Before this table existed, the same tool had *two* vocabularies: the harness
// dispatcher spelled sleep `sleep | terminal__sleep | wait`, while the steer
// arbitrator's subtask-action grammar spelled it
// `sleep | sleeptask | wait | waitseconds | pause`. A name accepted on one path
// was rejected on the other. The full alias arms were also re-typed three times
// inside the harness dispatcher. This table is now the single owner: every site
// that recognizes a tool alias must ask this table.
//
// Review decision on the divergent sleep set (deliberate union, not an accident):
// `sleeptask`, `waitseconds` and `pause` existed **only** in the steer path. They
// are folded into the shared table, so the harness dispatcher accepts them too.
// Rationale: all three name the same tool (`sleep`), they were already reachable
// from a model-facing spelling in production, and keeping them steer-only would
// preserve the exact defect this table removes. They are ordinary names a
// dispatcher can serve; nothing security-bearing is widened by accepting them
// (the sleep handler is the least privileged built-in, and the specialist
// allowlist gate still applies to them exactly like `wait`).
// Consequence worth knowing: alias spellings also reserve their MCP namespace —
// the registration policy refuses an MCP server whose config key is `pause`,
// `sleeptask` or `waitseconds`, exactly as it already refused `wait`, `sh`,
// `run`, `read`, `write`, `ls`, `grep`, …

/// Every canonical spelling a built-in handler answers to **verbatim**: the
/// advertised bare names plus the caesar-style `terminal__` variants.
pub const CANONICAL_TOOL_NAMES: &[&str] = &[
    TOOL_DELEGATE_TASK,
    TOOL_CREATE_PLAN,
    TOOL_ARCHIVE_PLAN,
    TOOL_READ_FILE,
    TERMINAL_READ_FILE,
    TOOL_WRITE_FILE,
    TERMINAL_WRITE_FILE,
    TOOL_REPLACE,
    TERMINAL_REPLACE,
    TOOL_RUN_COMMAND,
    TERMINAL_RUN_COMMAND,
    TOOL_GREP_SEARCH,
    TERMINAL_GREP_SEARCH,
    TOOL_GLOB,
    TERMINAL_GLOB,
    // `list_directory` / `terminal__list_directory` are NOT listed here: no dispatch
    // arm implements them, so claiming them here is what made them
    // claimed-but-unresolved (t-069 decision b — see the note on the constants).
    TOOL_SLEEP,
    TERMINAL_SLEEP,
    TOOL_REBIRTH,
    TOOL_LEAVE_VERDICT,
    TERMINAL_LEAVE_VERDICT,
    TOOL_REPLY_TO_ARBITRATOR,
    TOOL_PTY_SPAWN,
    TOOL_PTY_WRITE,
    TOOL_PTY_READ,
    TOOL_PTY_CLOSE,
    TOOL_PTY_LIST,
];

/// The alias table: `(accepted spelling, spelling it normalizes onto)`.
///
/// The normalized spelling is the one the dispatch gate (`normalize_tool_name`)
/// compares against: for the file/command families that is the `terminal__`
/// spelling, for the `pty_*` tools it is the bare spelling (there is no
/// `terminal__pty_*` spelling anywhere in the wire format).
///
/// Canonical spellings are deliberately **not** rows here — they resolve through
/// [`CANONICAL_TOOL_NAMES`] — so this table lists only names that are aliases of
/// something else. `normalize_tool_alias` consults both, in that order.
pub const TOOL_ALIAS_TABLE: &[(&str, &str)] = &[
    // read family
    (TOOL_READ_FILE, TERMINAL_READ_FILE),
    ("view_file", TERMINAL_READ_FILE),
    ("get_file", TERMINAL_READ_FILE),
    ("read", TERMINAL_READ_FILE),
    // write family
    (TOOL_WRITE_FILE, TERMINAL_WRITE_FILE),
    ("create_file", TERMINAL_WRITE_FILE),
    ("write_to_file", TERMINAL_WRITE_FILE),
    ("save_file", TERMINAL_WRITE_FILE),
    ("write", TERMINAL_WRITE_FILE),
    // replace family
    (TOOL_REPLACE, TERMINAL_REPLACE),
    ("replace_file_content", TERMINAL_REPLACE),
    ("edit_file", TERMINAL_REPLACE),
    // run_command family
    (TOOL_RUN_COMMAND, TERMINAL_RUN_COMMAND),
    ("execute_command", TERMINAL_RUN_COMMAND),
    ("run", TERMINAL_RUN_COMMAND),
    ("exec", TERMINAL_RUN_COMMAND),
    ("bash", TERMINAL_RUN_COMMAND),
    ("sh", TERMINAL_RUN_COMMAND),
    ("cmd", TERMINAL_RUN_COMMAND),
    // grep family
    (TOOL_GREP_SEARCH, TERMINAL_GREP_SEARCH),
    ("grep", TERMINAL_GREP_SEARCH),
    ("search", TERMINAL_GREP_SEARCH),
    // glob family
    (TOOL_GLOB, TERMINAL_GLOB),
    ("find_files", TERMINAL_GLOB),
    ("glob_search", TERMINAL_GLOB),
    // sleep family — `sleeptask`/`waitseconds`/`pause` joined in from the steer
    // path (see the review note at the top of this section).
    (TOOL_SLEEP, TERMINAL_SLEEP),
    ("wait", TERMINAL_SLEEP),
    ("sleeptask", TERMINAL_SLEEP),
    ("waitseconds", TERMINAL_SLEEP),
    ("pause", TERMINAL_SLEEP),
    // verdict family — the spellings `agents::validation::is_leave_verdict_tool`
    // used to enumerate privately (t-069). The table owns them now and the matcher
    // is a thin wrapper over [`is_leave_verdict_tool_name`], so there is exactly one
    // place where verdict-name spellings are enumerated. Namespaced spellings
    // (`validator__leave_verdict`, …) are *not* rows: they are covered by the one
    // suffix rule in [`LEAVE_VERDICT_NAME_SUFFIXES`] instead of by a row per
    // (hypothetical) namespace prefix.
    (TOOL_LEAVE_VERDICT, TERMINAL_LEAVE_VERDICT),
    ("leaveVerdict", TERMINAL_LEAVE_VERDICT),
    ("leave_verdict_tool", TERMINAL_LEAVE_VERDICT),
    // list_directory family — **removed** (t-069, decision b). No dispatch arm in
    // `src/harness/mod.rs` implements `list_directory`, `terminal__list_directory`,
    // `ls` or `list_files` (the only spelling ever mentioned there was a
    // normalization arm; the handler itself has never existed). A row here claims
    // the name for the built-in dispatcher *and* reserves it against MCP
    // registration, so every one of these spellings resolved to neither: the call
    // fell through MCP and came back `ToolError::UnknownTool`. Claiming a name no
    // handler serves is strictly worse than not claiming it, so the rows are gone;
    // an MCP server is now free to publish the name, and a call that nobody serves
    // is reported as unknown instead of being blocked by a phantom built-in claim.
    // pty family — the `pty__` spelling was accepted by the dispatcher arms and
    // by the built-in name table, but was missing from the normalizer, so it
    // could not pass the specialist allowlist gate. Same defect, same fix.
    ("pty__spawn", TOOL_PTY_SPAWN),
    ("pty__write", TOOL_PTY_WRITE),
    ("pty__read", TOOL_PTY_READ),
    ("pty__close", TOOL_PTY_CLOSE),
    ("pty__list", TOOL_PTY_LIST),
];

/// `true` when `name` is one of the canonical spellings in
/// [`CANONICAL_TOOL_NAMES`].
pub fn is_canonical_tool_name(name: &str) -> bool {
    CANONICAL_TOOL_NAMES.contains(&name)
}

/// Resolve `name` through the single alias table.
///
/// `Some(canonical)` for a canonical spelling (mapped to itself) or for an alias
/// in [`TOOL_ALIAS_TABLE`]; `None` for anything else (an MCP composed name, an
/// unknown name, …) — callers must then use the name verbatim.
pub fn normalize_tool_alias(name: &str) -> Option<&'static str> {
    if let Some((_, canonical)) = TOOL_ALIAS_TABLE.iter().find(|(alias, _)| *alias == name) {
        return Some(canonical);
    }
    CANONICAL_TOOL_NAMES
        .iter()
        .find(|canonical| **canonical == name)
        .copied()
}

/// `name` normalized through the alias table, or verbatim when the table has no
/// opinion about it. This is the spelling the dispatch arms match on.
pub fn canonical_tool_spelling(name: &str) -> &str {
    normalize_tool_alias(name).unwrap_or(name)
}

/// Every accepted spelling that resolves onto `canonical` — the alias rows whose
/// target is `canonical`, plus `canonical` itself when the table knows it. The
/// argument may be either the bare or the normalized spelling of the tool.
pub fn tool_spellings_for(canonical: &str) -> Vec<&'static str> {
    let resolved = normalize_tool_alias(canonical);
    let target = resolved.unwrap_or(canonical);
    let mut out: Vec<&'static str> = TOOL_ALIAS_TABLE
        .iter()
        .filter(|(_, target_row)| *target_row == target)
        .map(|(alias, _)| *alias)
        .collect();
    if let Some(resolved) = resolved
        && !out.contains(&resolved)
    {
        out.push(resolved);
    }
    out
}

/// `true` when `name` names the sleep tool in any spelling the shared table
/// accepts (exact comparison — this is the dispatcher-facing predicate).
pub fn is_sleep_tool_name(name: &str) -> bool {
    normalize_tool_alias(name) == Some(TERMINAL_SLEEP)
}

/// Fold a spelling for the grammar-tolerant comparison used by grammars that
/// strip separators and case (the steer subtask-action key folds `action` the
/// same way before comparing it).
pub fn fold_tool_spelling(name: &str) -> String {
    name.chars()
        .filter(|c| !matches!(c, '_' | '-' | ' ' | '\t'))
        .collect::<String>()
        .to_ascii_lowercase()
}

/// Grammar-tolerant counterpart of [`is_sleep_tool_name`]: `true` when a folded
/// action spelling names the sleep tool, e.g. `Terminal_Sleep`, `WAIT SECONDS` or
/// `sleeptask`. Used by the steer path so it cannot drift away from the harness
/// vocabulary again.
pub fn is_sleep_tool_grammar_spelling(name: &str) -> bool {
    let key = fold_tool_spelling(name);
    fold_tool_spelling(TERMINAL_SLEEP) == key
        || TOOL_ALIAS_TABLE
            .iter()
            .any(|(alias, target)| *target == TERMINAL_SLEEP && fold_tool_spelling(alias) == key)
}

// ── the verdict family (t-069) ──────────────────────────────────────────────
//
// The **one** place verdict-name spellings are enumerated: the exact spellings are
// the verdict rows of [`TOOL_ALIAS_TABLE`], and the only structural rule the
// validator ever needed is [`LEAVE_VERDICT_NAME_SUFFIXES`].
// [`crate::agents::validation::is_leave_verdict_tool`] is a thin wrapper over
// [`is_leave_verdict_tool_name`] — callers must not compare verdict names
// themselves, and no second verdict vocabulary may exist.

/// Structural rule for namespaced verdict spellings: a name ending in one of these
/// suffixes names the verdict tool (`terminal__leave_verdict`,
/// `validator__leave_verdict`, `legacy_leave_verdict`). The `__` form is implied by
/// the `_` form; both are listed so the intent (namespace-qualified *and*
/// underscore-qualified) is explicit and testable.
pub const LEAVE_VERDICT_NAME_SUFFIXES: &[&str] = &["__leave_verdict", "_leave_verdict"];

/// `true` when `name` names the verdict tool: either verbatim through the shared
/// alias table ([`TOOL_LEAVE_VERDICT`], [`TERMINAL_LEAVE_VERDICT`] and their alias
/// rows) or through the namespaced suffix rule. Case-tolerant like the matcher it
/// replaces, so `LEAVE_VERDICT` keeps working.
pub fn is_leave_verdict_tool_name(name: &str) -> bool {
    if normalize_tool_alias(name) == Some(TERMINAL_LEAVE_VERDICT) {
        return true;
    }
    let lower = name.to_ascii_lowercase();
    normalize_tool_alias(&lower) == Some(TERMINAL_LEAVE_VERDICT)
        || LEAVE_VERDICT_NAME_SUFFIXES
            .iter()
            .any(|suffix| lower.ends_with(suffix))
}

const fn const_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut i = 0;
    while i < a.len() {
        if a[i] != b[i] {
            return false;
        }
        i += 1;
    }
    true
}

const fn is_canonical_const(name: &str) -> bool {
    let mut i = 0;
    while i < CANONICAL_TOOL_NAMES.len() {
        if const_str_eq(CANONICAL_TOOL_NAMES[i], name) {
            return true;
        }
        i += 1;
    }
    false
}

const fn alias_rows_outside_canonical() -> usize {
    let mut n = 0;
    let mut i = 0;
    while i < TOOL_ALIAS_TABLE.len() {
        if !is_canonical_const(TOOL_ALIAS_TABLE[i].0) {
            n += 1;
        }
        i += 1;
    }
    n
}

const BUILTIN_TABLE_LEN: usize = CANONICAL_TOOL_NAMES.len() + alias_rows_outside_canonical();

const fn build_builtin_table() -> [&'static str; BUILTIN_TABLE_LEN] {
    let mut out = [""; BUILTIN_TABLE_LEN];
    let mut i = 0;
    while i < CANONICAL_TOOL_NAMES.len() {
        out[i] = CANONICAL_TOOL_NAMES[i];
        i += 1;
    }
    let mut j = 0;
    let mut k = CANONICAL_TOOL_NAMES.len();
    while j < TOOL_ALIAS_TABLE.len() {
        let alias = TOOL_ALIAS_TABLE[j].0;
        if !is_canonical_const(alias) {
            out[k] = alias;
            k += 1;
        }
        j += 1;
    }
    out
}

const BUILTIN_TABLE: [&str; BUILTIN_TABLE_LEN] = build_builtin_table();

/// Every name the built-in dispatcher claims: the canonical spellings plus every
/// alias spelling in [`TOOL_ALIAS_TABLE`].
///
/// **Derived, never re-typed** — the harness re-exports this as
/// `crate::harness::BUILTIN_TOOL_NAMES`, which is the table the MCP name policy
/// is built on (t-034c). Adding an alias to [`TOOL_ALIAS_TABLE`] therefore also
/// reserves that spelling against MCP registration; forgetting that link is what
/// let an alias be dispatchable but unregistered (or the reverse).
pub const BUILTIN_TOOL_NAME_TABLE: &[&str] = &BUILTIN_TABLE;

#[cfg(test)]
mod tests {
    use super::*;

    /// (b) Every canonical `TOOL_*` / `TERMINAL_*` constant round-trips through
    /// the shared table, and every one of them is claimed by the derived
    /// built-in name table.
    #[test]
    fn every_canonical_tool_constant_round_trips_through_the_table() {
        let canonical: &[&str] = &[
            TOOL_DELEGATE_TASK,
            TOOL_CREATE_PLAN,
            TOOL_ARCHIVE_PLAN,
            TOOL_READ_FILE,
            TOOL_WRITE_FILE,
            TOOL_REPLACE,
            TOOL_RUN_COMMAND,
            TOOL_GREP_SEARCH,
            TOOL_GLOB,
            TOOL_REBIRTH,
            TOOL_LEAVE_VERDICT,
            TOOL_REPLY_TO_ARBITRATOR,
            TOOL_SLEEP,
            TOOL_PTY_SPAWN,
            TOOL_PTY_WRITE,
            TOOL_PTY_READ,
            TOOL_PTY_CLOSE,
            TOOL_PTY_LIST,
            TERMINAL_READ_FILE,
            TERMINAL_WRITE_FILE,
            TERMINAL_REPLACE,
            TERMINAL_RUN_COMMAND,
            TERMINAL_GREP_SEARCH,
            TERMINAL_GLOB,
            TERMINAL_SLEEP,
            TERMINAL_LEAVE_VERDICT,
        ];
        for name in canonical {
            assert!(
                is_canonical_tool_name(name),
                "'{name}' must be listed in CANONICAL_TOOL_NAMES"
            );
            let resolved = normalize_tool_alias(name);
            assert!(
                resolved.is_some(),
                "'{name}' must resolve through the alias table"
            );
            assert!(
                BUILTIN_TOOL_NAME_TABLE.contains(name),
                "'{name}' must be claimed by BUILTIN_TOOL_NAME_TABLE"
            );
            // A canonical tool always names itself through the sleep helper only
            // for the sleep family; for the others the resolved spelling must be
            // a name the dispatcher claims verbatim.
            let resolved = resolved.unwrap_or(name);
            assert!(
                BUILTIN_TOOL_NAME_TABLE.contains(&resolved),
                "'{name}' resolves to '{resolved}', which the built-in table must claim"
            );
        }
    }

    /// The alias table is total and unambiguous: alias keys are unique, every
    /// target is a name the built-in table claims, and no alias is itself a
    /// canonical spelling of a *different* tool's row target chain.
    #[test]
    fn alias_table_rows_are_unique_and_point_at_claimed_names() {
        let mut seen: Vec<&str> = Vec::new();
        for (alias, target) in TOOL_ALIAS_TABLE {
            assert!(
                !seen.contains(alias),
                "alias '{alias}' appears twice in the table"
            );
            seen.push(alias);
            assert_ne!(alias, target, "row ('{alias}', …) must not alias itself");
            assert!(
                is_canonical_tool_name(target) || TOOL_ALIAS_TABLE.iter().any(|(a, _)| a == target),
                "'{alias}' normalizes to '{target}', which is not a known spelling"
            );
            assert!(
                BUILTIN_TOOL_NAME_TABLE.contains(target),
                "'{alias}' normalizes to '{target}', which the built-in table must claim"
            );
        }
        // The derived built-in table is exactly canonical ∪ alias keys.
        assert_eq!(
            BUILTIN_TOOL_NAME_TABLE.len(),
            CANONICAL_TOOL_NAMES.len() + alias_rows_outside_canonical(),
            "the derived built-in table must hold every canonical name and every alias key"
        );
        for (alias, _) in TOOL_ALIAS_TABLE {
            assert!(
                BUILTIN_TOOL_NAME_TABLE.contains(alias),
                "alias '{alias}' must be claimed by the built-in table"
            );
        }
        assert!(
            BUILTIN_TOOL_NAME_TABLE
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == BUILTIN_TOOL_NAME_TABLE.len(),
            "the derived built-in table must not contain duplicates"
        );
    }

    /// (a) THE REGRESSION PIN: one alias set, two consumers. Every spelling the
    /// harness path resolves onto the sleep tool must be accepted by the steer
    /// path as the Sleep action, and vice versa — the divergent vocabularies this
    /// table replaces are exactly what this test would have caught.
    #[test]
    fn sleep_vocabulary_is_identical_on_the_harness_and_steer_paths() {
        // Spellings that must be recognised by *both* paths.
        for spelling in tool_spellings_for(TERMINAL_SLEEP) {
            assert!(
                is_sleep_tool_name(spelling),
                "harness path must treat '{spelling}' as the sleep tool"
            );
            assert!(
                is_sleep_tool_grammar_spelling(spelling),
                "steer path must treat '{spelling}' as the sleep tool"
            );
            // The steer path folds the raw action before comparing, exactly like
            // `SteerSubtaskAction::from_str`; the folded form must still resolve.
            let folded = fold_tool_spelling(spelling);
            assert!(
                is_sleep_tool_grammar_spelling(&folded),
                "steer path must treat the folded spelling '{folded}' as sleep"
            );
        }

        // Harness-side: the shared normalizer maps every sleep spelling onto the
        // one canonical spelling, so the dispatcher arm matches once.
        for spelling in tool_spellings_for(TERMINAL_SLEEP) {
            assert_eq!(
                canonical_tool_spelling(spelling),
                TERMINAL_SLEEP,
                "'{spelling}' must normalize to '{TERMINAL_SLEEP}'"
            );
        }

        // Neither path may accept a near-miss: what the table does not know stays
        // unknown on *both* sides.
        for near_miss in ["sleepy", "sleeping", "await", "paused", "waits", "", "nap"] {
            assert!(
                !is_sleep_tool_name(near_miss),
                "harness path must not treat {near_miss:?} as the sleep tool"
            );
            assert!(
                !is_sleep_tool_grammar_spelling(near_miss),
                "steer path must not treat {near_miss:?} as the sleep tool"
            );
        }
    }

    /// A name the table does not know is passed through verbatim — MCP composed
    /// names must never be renamed by the alias layer.
    #[test]
    fn unknown_names_are_not_normalized() {
        for name in [
            "myserver__do_thing",
            "filesystem__read_file",
            "remote__grep",
            "terminal__run__command",
            "pause_and_think",
            "read_my_file",
            "",
        ] {
            assert!(
                normalize_tool_alias(name).is_none(),
                "'{name}' must not resolve through the alias table"
            );
            assert_eq!(canonical_tool_spelling(name), name);
        }
    }

    /// Source guard for the collapse: the routed call sites must not keep any
    /// hand-typed tool-name string literal of this vocabulary — neither an
    /// alias arm nor a quoted canonical name. Needles are derived from the
    /// table at runtime so a newly added alias row is guarded automatically.
    #[test]
    fn routed_call_sites_do_not_retype_tool_alias_literals() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let routed_files = ["src/harness/mod.rs", "src/orchestrator/steer.rs"];
        let mut needles: Vec<String> = TOOL_ALIAS_TABLE
            .iter()
            .map(|(alias, _)| format!("\"{alias}\""))
            .collect();
        for canonical in CANONICAL_TOOL_NAMES {
            needles.push(format!("\"{canonical}\""));
        }
        assert!(!needles.is_empty());
        let mut audited = 0usize;
        for file in routed_files {
            let path = root.join(file);
            let source = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()));
            for needle in &needles {
                assert!(
                    !source.contains(needle.as_str()),
                    "{file} still hard-codes the tool-name literal {needle} — it must be routed \
                     through crate::tool_names (constants, TOOL_ALIAS_TABLE, \
                     normalize_tool_alias / canonical_tool_spelling / is_sleep_tool_name / \
                     is_sleep_tool_grammar_spelling)"
                );
            }
            audited += 1;
        }
        assert_eq!(audited, routed_files.len());
    }

    /// (t-069 item 1) The verdict family lives here and nowhere else: the table
    /// rows plus one suffix rule cover every accepted spelling, and the validator's
    /// public matcher agrees with the table on all of them.
    #[test]
    fn verdict_family_is_owned_by_the_table() {
        let family = tool_spellings_for(TERMINAL_LEAVE_VERDICT);
        for spelling in &family {
            assert_eq!(normalize_tool_alias(spelling), Some(TERMINAL_LEAVE_VERDICT));
            assert!(
                is_leave_verdict_tool_name(spelling),
                "'{spelling}' is a verdict row and must be recognized"
            );
            assert!(
                crate::agents::validation::is_leave_verdict_tool(spelling),
                "the validator matcher must accept the table's verdict spelling '{spelling}'"
            );
        }
        // Namespaced spellings come from the suffix rule, never from a per-prefix row.
        for namespaced in [
            "validator__leave_verdict",
            "auditor__leave_verdict",
            "legacy_leave_verdict",
        ] {
            assert!(
                normalize_tool_alias(namespaced).is_none(),
                "'{namespaced}' is not a spelling of the table — only the suffix rule knows it"
            );
            assert!(is_leave_verdict_tool_name(namespaced));
        }
        // The bare verdict name must not be mistaken for the sleep family, and a
        // verdict-ish name the rule does not cover stays unknown.
        for near_miss in ["verdict", "leave", "leave_verdicts", "read_verdict", ""] {
            assert!(
                !is_leave_verdict_tool_name(near_miss),
                "{near_miss:?} is not a verdict tool"
            );
        }
    }

    /// (t-069 item 3, decision pin) `list_directory`, `terminal__list_directory`,
    /// `ls` and `list_files` are **not** claimed: no dispatch arm in
    /// `src/harness/mod.rs` implements them (READ-ONLY evidence, `dispatch`,
    /// `dispatch_manager`, `dispatch_specialist`, `dispatch_specialist_async` —
    /// none has a `*_LIST_DIRECTORY` arm), so the table rows that used to claim
    /// them made every one of them claimed-but-unresolved. The decision is: the
    /// rows are gone, and the end-to-end outcome is asserted here.
    #[test]
    fn list_directory_spellings_are_unclaimed_and_report_unknown_tool() {
        let unclaimed = [
            TOOL_LIST_DIRECTORY,
            TERMINAL_LIST_DIRECTORY,
            "ls",
            "list_files",
        ];
        for name in unclaimed {
            assert!(
                !is_canonical_tool_name(name),
                "'{name}' must not be a canonical built-in spelling"
            );
            assert!(
                normalize_tool_alias(name).is_none(),
                "'{name}' must not resolve through the alias table"
            );
            assert!(
                !BUILTIN_TOOL_NAME_TABLE.contains(&name),
                "'{name}' must not be claimed by the built-in name table (it would also be \
                 refused at MCP registration while no handler serves it)"
            );
            // End-to-end through the real dispatcher: not a built-in, not routed to
            // MCP (a composed MCP name always carries `__`), so it is reported as an
            // unknown tool under the exact spelling the caller used.
            let outcome = crate::harness::dispatch(&crate::harness::ToolInvocation {
                name: name.to_string(),
                arguments: serde_json::json!({}),
            });
            assert!(
                matches!(&outcome,
                    Err(crate::harness::ToolError::UnknownTool(reported)) if reported == name),
                "'{name}' has no built-in handler and must end in UnknownTool, got {outcome:?}"
            );
        }
    }

    /// (t-069 item 3, the guard that keeps the decision from regressing) The other
    /// direction of the same rule: every alias the table **does** claim must reach a
    /// dispatch arm, i.e. must never come back as `UnknownTool`. Probed with the
    /// handlers that refuse on missing arguments before touching anything (no shell,
    /// no sleep timer, no plan archive, no orchestrator, no arbitrator, no PTY
    /// spawn), so the probe is hermetic.
    #[test]
    fn claimed_aliases_never_fall_through_to_unknown_tool() {
        for alias in [
            "view_file",
            "get_file",
            "read",
            "write_to_file",
            "save_file",
            "create_file",
            "replace_file_content",
            "edit_file",
            "grep",
            "search",
            "find_files",
            "glob_search",
            "pty__read",
            "pty__write",
            "pty__close",
            "pty__list",
        ] {
            assert!(
                normalize_tool_alias(alias).is_some(),
                "'{alias}' must be a row of the alias table for this probe to mean anything"
            );
            // `block_on_safe` mirrors what the real callers do: some handlers touch
            // tokio-backed globals (the PTY manager) and must be probed inside a runtime.
            let outcome = crate::harness::block_on_safe(async move {
                crate::harness::dispatch(&crate::harness::ToolInvocation {
                    name: alias.to_string(),
                    arguments: serde_json::json!({}),
                })
            });
            assert!(
                !matches!(outcome, Err(crate::harness::ToolError::UnknownTool(_))),
                "'{alias}' is claimed by the table and must reach a dispatch arm, got {outcome:?}"
            );
        }
    }
}
