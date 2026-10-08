//! Single owner of "render a one-line human preview of a tool call's JSON
//! arguments" (dedup cluster: preview/format twins).
//!
//! Two independent copies of this rendering used to exist:
//! * `agents::runner::formatting` — the bare *value* preview used for the
//!   specialist status line and the `invoking tool:` log line;
//! * `ui::helpers` — the decorated `name(args)` line used for the TUI tool-call
//!   row and the transcript.
//!
//! Both now delegate here. The module keeps the **union** of the two renders
//! (no tool-specific formatting was dropped) while centralising everything that
//! both copies knew independently: which argument key carries which value per
//! tool, the legacy aliases that are still emitted in practice, and the
//! char-boundary-safe clipping used to keep a preview on one line.
//!
//! ## Argument-key authority
//! Keys follow the wire schema in [`crate::types::tools`] and the harness
//! implementations that actually execute the call (e.g.
//! [`crate::harness::search::grep_search`] reads `pattern`). The one tolerated
//! legacy alias is `query` for the search tool: it is still produced by the XML
//! rescue path in [`crate::harness::monitor::xml`] and is accepted at execution
//! time by [`crate::harness::fs::str_arg`], so the preview accepts it as a
//! fallback *after* the canonical `pattern` key.
//!
//! All clipping goes through [`crate::text_util::truncate_chars`], so a preview
//! can never split a multi-byte character (Swedish `å`/`ä`/`ö`, emoji, CJK, …)
//! and never panics on a non-char boundary.
//!
//! ## Sleep-argument authority (dedup gate t-044, sweep C; refreshed t-070)
//! Beyond rendering, this module is also the single owner of `sleep`-argument
//! *extraction*: [`sleep_duration_secs`] resolves the duration key(s), every
//! JSON number shape, the fallback default and the min/max clamp. The two
//! harness handlers (`handle_sleep`, `handle_sleep_async`) used to carry three
//! divergent copies of that canonicalization; they now call this owner, so the
//! sync and async tool paths cannot disagree about what an input means.
//!
//! The owner is one function plus one constant trio, and both are `pub` so no
//! other subsystem may re-type the numbers:
//! * [`sleep_duration_secs`] — key lookup → number-shape parsing → default →
//!   clamp, in one place;
//! * [`SLEEP_DEFAULT_SECS`] — what a call with no usable duration waits;
//! * [`SLEEP_MIN_SECS`] / [`SLEEP_MAX_SECS`] — the bounds every *parsed*
//!   duration is clamped into (`0` → `SLEEP_MIN_SECS`, `301` → `SLEEP_MAX_SECS`).
//!
//! Every caller that decides "how long may this sleep be" now asks this owner
//! for the vocabulary: the harness sleep handlers, the steered sleep
//! (`orchestrator::steer` calls [`sleep_duration_secs`] / [`SLEEP_DEFAULT_SECS`]
//! instead of its old unclamped `unwrap_or(5)` copy) and the steering
//! arbitrator's **lower** bound (`ui::bridge::arbiter` floors a requested sleep
//! at [`SLEEP_MIN_SECS`], defaulting it to [`SLEEP_DEFAULT_SECS`]). Only the
//! arbitrator's *upper* budget knobs stay separate on purpose — they bound one
//! steering instruction's chained sleep, not the `sleep` tool's own ceiling, and
//! are deliberately not derived from [`SLEEP_MAX_SECS`].

use crate::text_util::truncate_chars;
use crate::tool_names::{
    TOOL_CREATE_PLAN, TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_READ_FILE,
    TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_SLEEP, TOOL_WRITE_FILE,
};
use serde_json::Value;

/// Ellipsis appended by [`clip`] when a value had to be shortened.
const ELLIPSIS: &str = "…";
/// How many characters of the budget are reserved for the clip marker. The
/// single-character ellipsis is rendered one-wide but historically the cut left
/// three characters of slack (matching the width of `...`), so the visible
/// budget is `budget - CLIP_SLACK` characters.
const CLIP_SLACK: usize = 3;

/// Character budget for the `run_command` value in the compact preview.
const COMMAND_PREVIEW_BUDGET: usize = 40;
/// Character budget for the serialized-JSON fallback in the compact preview.
const GENERIC_PREVIEW_BUDGET: usize = 30;
/// Character budget for the serialized argument text in the decorated preview.
const DECORATED_BUDGET: usize = 60;

// ---- sleep-duration authority (dedup gate t-044 sweep C) -------------------

/// Fallback sleep duration (seconds) when no duration key is present or the
/// value is not a usable duration. Single owner of the default/clamp trio **for
/// the `sleep` tool path** (the harness handlers plus the previews in this
/// module); the harness sleep handlers must not re-type these literals.
pub const SLEEP_DEFAULT_SECS: u64 = 5;
/// Lower bound applied to any parsed sleep duration.
pub const SLEEP_MIN_SECS: u64 = 1;
/// Upper bound applied to any parsed sleep duration — caps a sleep at 5 minutes.
pub const SLEEP_MAX_SECS: u64 = 300;

// The single default/clamp pair must stay internally consistent; checked at
// compile time so a retyped constant cannot silently invert the range.
const _: () = assert!(SLEEP_MIN_SECS <= SLEEP_DEFAULT_SECS && SLEEP_DEFAULT_SECS <= SLEEP_MAX_SECS);

/// Render the bare *value* preview of a tool call's arguments (compact form,
/// used for status lines and progress text where the caller already prints the
/// tool name).
pub fn preview_tool_args(name: &str, args: &Value) -> String {
    match name {
        TOOL_READ_FILE | TOOL_WRITE_FILE | TOOL_REPLACE => path_arg(args).to_string(),
        TOOL_RUN_COMMAND => clip(command_arg(args), COMMAND_PREVIEW_BUDGET),
        TOOL_GREP_SEARCH => search_pattern(args).to_string(),
        TOOL_GLOB => glob_pattern(args).to_string(),
        TOOL_SLEEP => format!("{}s", sleep_duration_secs(args)),
        _ => clip(&args.to_string(), GENERIC_PREVIEW_BUDGET),
    }
}

/// Render the decorated one-line preview `name(args)` used by the UI tool-call
/// row and the transcript.
pub fn preview_tool_call(name: &str, args: &Value) -> String {
    let body = match name {
        TOOL_GREP_SEARCH => match args.get("path").and_then(Value::as_str) {
            Some(path) => format!("{} in {path}", search_pattern(args)),
            None => search_pattern(args).to_string(),
        },
        TOOL_GLOB => glob_pattern(args).to_string(),
        TOOL_READ_FILE | TOOL_WRITE_FILE | TOOL_REPLACE => path_arg(args).to_string(),
        TOOL_RUN_COMMAND => clip(command_arg(args), DECORATED_BUDGET),
        TOOL_CREATE_PLAN => format!("plan_markdown: {} chars", plan_markdown_len(args)),
        TOOL_DELEGATE_TASK => {
            let agent = delegate_agent(args);
            let task_id = delegate_task_id(args);
            if task_id.is_empty() {
                format!("agent: {agent}")
            } else {
                format!("agent: {agent}, task_id: {task_id}")
            }
        }
        // Everything else (including `sleep`) keeps the raw serialized arguments.
        _ => return format!("{name}({})", clip(&args.to_string(), DECORATED_BUDGET)),
    };
    format!("{name}({body})")
}

/// Render the verbose argument form logged with the `invoking tool:` line, where
/// sizes of large payload fields (`content`, `old_str`, `new_str`,
/// `plan_markdown`) matter more than their text.
pub fn full_tool_args(name: &str, args: &Value) -> String {
    match name {
        TOOL_WRITE_FILE => match args.get("path").and_then(Value::as_str) {
            Some(path) => {
                let len = byte_len(args.get("content"));
                format!("{path} (content: {len} bytes)")
            }
            None => args.to_string(),
        },
        TOOL_REPLACE => match args.get("path").and_then(Value::as_str) {
            Some(path) => {
                let old_len = byte_len(args.get("old_str"));
                let new_len = byte_len(args.get("new_str"));
                format!("{path} (replace: {old_len}b -> {new_len}b)")
            }
            None => args.to_string(),
        },
        TOOL_READ_FILE => match args.get("path").and_then(Value::as_str) {
            Some(path) => path.to_string(),
            None => args.to_string(),
        },
        TOOL_RUN_COMMAND => command_arg(args).to_string(),
        TOOL_GREP_SEARCH => {
            let pattern = search_pattern(args);
            let path = path_arg(args);
            if path.is_empty() {
                format!("query=\"{pattern}\"")
            } else {
                format!("query=\"{pattern}\", path=\"{path}\"")
            }
        }
        TOOL_GLOB => glob_pattern(args).to_string(),
        TOOL_DELEGATE_TASK => {
            let agent = delegate_agent(args);
            let task_id = delegate_task_id(args);
            let prompt = delegate_prompt(args);
            format!("agent={agent}, task_id={task_id}, prompt=\"{prompt}\"")
        }
        name if crate::agents::validation::is_leave_verdict_tool(name) => {
            let verdict = first_str(args, &["verdict", "status", "decision"]);
            let comments = first_str(args, &["comments", "comment", "feedback"]);
            format!("verdict={verdict}, comments=\"{comments}\"")
        }
        TOOL_SLEEP => {
            let secs = sleep_duration_secs(args);
            let reason = first_str(args, &["reason"]);
            if reason.is_empty() {
                format!("seconds={secs}")
            } else {
                format!("seconds={secs}, reason=\"{reason}\"")
            }
        }
        _ => args.to_string(),
    }
}

/// Canonical search-pattern key for the grep/search tool.
///
/// The schema ([`crate::types::tools`]) and the executor
/// ([`crate::harness::search::grep_search`]) both use `pattern`; `query` is kept
/// only as a trailing alias because the XML rescue path still emits it and
/// [`crate::harness::fs::str_arg`] tolerates it at execution time.
pub fn search_pattern(args: &Value) -> &str {
    first_str(args, &["pattern", "query"])
}

/// Pattern key of the glob tool — strictly `pattern`, no aliases (the two
/// renders this module replaced never guessed at glob aliases either).
fn glob_pattern(args: &Value) -> &str {
    first_str(args, &["pattern"])
}

/// `path` argument shared by the file tools.
fn path_arg(args: &Value) -> &str {
    first_str(args, &["path"])
}

/// `command` argument of the command tool.
fn command_arg(args: &Value) -> &str {
    first_str(args, &["command"])
}

/// Length of the plan payload in a `create_plan` call.
///
/// Counted in **bytes**, exactly as the `ui::helpers` render this replaced did
/// (the `chars` wording of that render's label is kept verbatim to avoid any
/// behaviour drift; see the note in the task report).
fn plan_markdown_len(args: &Value) -> usize {
    byte_len(args.get("plan_markdown"))
}

/// Agent name of a delegation (defaults to `specialist` when the key is absent,
/// matching the schema default).
fn delegate_agent(args: &Value) -> &str {
    args.get("agent_name")
        .and_then(Value::as_str)
        .unwrap_or("specialist")
}

/// Task id of a delegation.
fn delegate_task_id(args: &Value) -> &str {
    first_str(args, &["task_id"])
}

/// Prompt text of a delegation.
fn delegate_prompt(args: &Value) -> &str {
    first_str(args, &["prompt"])
}

/// The **single owner** of `sleep`-argument extraction (dedup gate t-044,
/// sweep C: three divergent copies of this canonicalization used to exist —
/// `harness::sleep::handle_sleep`, `harness::sleep::handle_sleep_async` and the
/// preview render below).
///
/// Returns the duration in seconds that a `sleep` call will actually wait, i.e.
/// the whole pipeline in one place: key lookup → number-shape parsing → default
/// → clamp. The sync and async harness handlers call this and nothing else, so
/// an input accepted by one path is by construction accepted (with the same
/// value) by the other.
///
/// Accepted keys — the aliases the harness has always tolerated: `seconds`,
/// then `duration`, then `duration_seconds` (first key present wins).
///
/// Accepted value shapes:
/// * JSON unsigned integers (`5`), including integers too large for `i64`;
/// * JSON signed integers (`-3` is *not* a duration — see below);
/// * JSON floats *inside the tool's own range*: finite floats within
///   [`SLEEP_MIN_SECS`]..=[`SLEEP_MAX_SECS`] truncated toward zero (`2.5` → `2`,
///   `6.0` → `6`, `299.9` → `299`, `300.0` → `300`). Truncation is the safer
///   direction: it can never sleep *longer* than the caller asked. Out-of-range
///   floats (`0.5`, `300.9`, `1e9`, `f64::MAX`) are *not* durations and fall
///   back to the default — `serde_json` never reports a float through
///   `as_u64`/`as_i64`, so before this owner existed no copy accepted any float
///   spelling at all and each of them resolved to the default; clamping them to
///   the ceiling instead would have made a malformed float sleep 60x longer
///   than before.
/// * string-encoded unsigned integers (`"5"`) — exactly what the previous copies
///   accepted via `parse::<u64>()`; `"2.5"`/`"-3"`/`"5s"` stay unparseable.
///
/// Unusable values (missing key, `null`, `bool`, object/array, non-positive
/// integers such as `-3`, floats outside the range above, unparseable strings)
/// fall back to [`SLEEP_DEFAULT_SECS`]. A *parsed* value is then clamped into
/// [`SLEEP_MIN_SECS`]..=[`SLEEP_MAX_SECS`] (`0` → `1`, `301`/`u64::MAX` → `300`).
pub fn sleep_duration_secs(args: &Value) -> u64 {
    raw_sleep_secs(args)
        .unwrap_or(SLEEP_DEFAULT_SECS)
        .clamp(SLEEP_MIN_SECS, SLEEP_MAX_SECS)
}

/// The parsed-but-undefaulted duration of a `sleep` call, shared by every
/// duration-key spelling and JSON number shape documented on
/// [`sleep_duration_secs`].
fn raw_sleep_secs(args: &Value) -> Option<u64> {
    args.get("seconds")
        .or_else(|| args.get("duration"))
        .or_else(|| args.get("duration_seconds"))
        .and_then(parse_secs_value)
}

/// Parse one JSON value into a raw (unclamped) number of seconds.
fn parse_secs_value(value: &Value) -> Option<u64> {
    // Integers: unsigned first, so `u64::MAX`-sized values survive (verified
    // against `serde_json`: `as_u64`/`as_i64` never report a float, so the
    // float arm below is the only route for `5.0`, `2.5`, `1e9`, …).
    if let Some(secs) = value.as_u64() {
        return Some(secs);
    }
    // Negative integers are not durations: they must fall back to the default
    // rather than be clamped up to `SLEEP_MIN_SECS` (the behaviour every copy
    // had, via the `i > 0` guard of the sync path).
    if let Some(secs) = value.as_i64() {
        return if secs > 0 { Some(secs as u64) } else { None };
    }
    // Fractional durations: only floats already inside the tool's own range are
    // accepted, truncated toward zero (`2.5` -> `2`, `299.9` -> `299`).
    // Out-of-range floats (`0.5`, `300.9`, `1e9`, `f64::MAX`) are treated as
    // unusable and fall back to the default — which is exactly what every
    // pre-existing copy did, since `serde_json` never reported a float through
    // `as_u64`/`as_i64`. Accepting them and clamping them up to the ceiling
    // would make a malformed float sleep 60x longer than the default, i.e.
    // strictly *less* safe than the behaviour being replaced.
    if let Some(secs) = value.as_f64() {
        return if secs.is_finite() && secs >= SLEEP_MIN_SECS as f64 && secs <= SLEEP_MAX_SECS as f64
        {
            Some(secs as u64)
        } else {
            None
        };
    }
    // String-encoded numbers, exactly as tolerated before (`parse::<u64>`).
    value.as_str().and_then(|s| s.parse().ok())
}

/// First non-`null` string value found under any of `keys`.
fn first_str<'a>(args: &'a Value, keys: &[&str]) -> &'a str {
    keys.iter()
        .find_map(|key| args.get(*key).and_then(Value::as_str))
        .unwrap_or("")
}

/// Byte length of an optional string field (reported as *bytes* by the callers).
fn byte_len(value: Option<&Value>) -> usize {
    value.map_or(0, |v| v.as_str().map_or(0, str::len))
}

/// Clip `s` to `budget` characters for single-line display, appending
/// [`ELLIPSIS`] when clipping happened. Char-boundary-safe: never splits a
/// multi-byte character and never panics.
fn clip(s: &str, budget: usize) -> String {
    if s.chars().count() <= budget {
        return s.to_string();
    }
    format!(
        "{}{ELLIPSIS}",
        truncate_chars(s, budget.saturating_sub(CLIP_SLACK))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ---- grep/search reads the schema key `pattern` (bug regression) ------

    #[test]
    fn tool_args_grep_preview_is_non_empty_with_pattern_key() {
        let args = json!({ "pattern": "TODO", "path": "src" });
        assert_eq!(preview_tool_args(TOOL_GREP_SEARCH, &args), "TODO");
        assert_eq!(
            preview_tool_call(TOOL_GREP_SEARCH, &args),
            "grep_search(TODO in src)"
        );
        assert_eq!(
            full_tool_args(TOOL_GREP_SEARCH, &args),
            "query=\"TODO\", path=\"src\""
        );
        assert_eq!(search_pattern(&args), "TODO");
    }

    #[test]
    fn tool_args_grep_preview_without_path_omits_location() {
        let args = json!({ "pattern": "needle" });
        assert_eq!(
            preview_tool_call(TOOL_GREP_SEARCH, &args),
            "grep_search(needle)"
        );
        assert_eq!(full_tool_args(TOOL_GREP_SEARCH, &args), "query=\"needle\"");
    }

    #[test]
    fn tool_args_grep_preview_accepts_legacy_query_alias() {
        // Still emitted by `harness::monitor::xml` rescue and tolerated by
        // `harness::fs::str_arg`, so it must render — but `pattern` wins.
        let alias = json!({ "query": "Smith" });
        assert_eq!(preview_tool_args(TOOL_GREP_SEARCH, &alias), "Smith");
        assert_eq!(
            preview_tool_call(TOOL_GREP_SEARCH, &alias),
            "grep_search(Smith)"
        );
        let both = json!({ "pattern": "canon", "query": "legacy" });
        assert_eq!(preview_tool_args(TOOL_GREP_SEARCH, &both), "canon");
        assert_eq!(search_pattern(&both), "canon");
    }

    #[test]
    fn tool_args_glob_preview_uses_pattern_key() {
        let args = json!({ "pattern": "**/*.rs" });
        assert_eq!(preview_tool_args(TOOL_GLOB, &args), "**/*.rs");
        assert_eq!(preview_tool_call(TOOL_GLOB, &args), "glob(**/*.rs)");
        assert_eq!(full_tool_args(TOOL_GLOB, &args), "**/*.rs");
    }

    // ---- unknown tool fallback -------------------------------------------

    #[test]
    fn tool_args_unknown_tool_falls_back_to_serialized_args() {
        let args = json!({ "k": 1 });
        assert_eq!(preview_tool_args("custom_tool", &args), "{\"k\":1}");
        assert_eq!(
            preview_tool_call("custom_tool", &args),
            "custom_tool({\"k\":1})"
        );
        assert_eq!(full_tool_args("custom_tool", &args), "{\"k\":1}");
    }

    #[test]
    fn tool_args_pty_tool_falls_back_to_serialized_args() {
        let args = json!({ "id": "dbg-1", "command": "gdb ./app" });
        // `serde_json` renders object keys in sorted order.
        assert_eq!(
            preview_tool_call(crate::tool_names::TOOL_PTY_SPAWN, &args),
            "pty_spawn({\"command\":\"gdb ./app\",\"id\":\"dbg-1\"})"
        );
    }

    // ---- empty / invalid / missing arguments ------------------------------

    #[test]
    fn tool_args_empty_and_invalid_args_render_without_panic() {
        for args in [
            Value::Null,
            json!([]),
            json!("truncated json {"),
            json!(42),
            json!(""),
        ] {
            // Must be infallible regardless of shape.
            let _ = preview_tool_args(TOOL_GREP_SEARCH, &args);
            let _ = preview_tool_call(TOOL_GREP_SEARCH, &args);
            let _ = full_tool_args(TOOL_GREP_SEARCH, &args);
        }
        assert_eq!(preview_tool_args(TOOL_GREP_SEARCH, &Value::Null), "");
        assert_eq!(
            preview_tool_call(TOOL_GREP_SEARCH, &Value::Null),
            "grep_search()"
        );
        assert_eq!(full_tool_args(TOOL_GREP_SEARCH, &Value::Null), "query=\"\"");
        assert_eq!(preview_tool_args(TOOL_RUN_COMMAND, &json!("")), "");
        assert_eq!(preview_tool_call(TOOL_READ_FILE, &json!({})), "read_file()");
        assert_eq!(preview_tool_args(TOOL_READ_FILE, &json!({})), "");
        // Missing keys fall back to the executor's defaults.
        assert_eq!(preview_tool_args(TOOL_SLEEP, &json!({})), "5s");
        assert_eq!(full_tool_args(TOOL_SLEEP, &json!({})), "seconds=5");
        // `null` values behave like missing ones.
        assert_eq!(
            preview_tool_args(TOOL_GREP_SEARCH, &json!({"pattern": null})),
            ""
        );
    }

    #[test]
    fn tool_args_write_replace_full_reports_sizes() {
        let replace = json!({
            "path": "cpu/ppc/jit/jit.cpp",
            "old_str": "int a = 1;",
            "new_str": "int a = 2; int b = 3;"
        });
        assert_eq!(
            full_tool_args(TOOL_REPLACE, &replace),
            "cpu/ppc/jit/jit.cpp (replace: 10b -> 21b)"
        );
        let write = json!({ "path": "test.txt", "content": "hello world" });
        assert_eq!(
            full_tool_args(TOOL_WRITE_FILE, &write),
            "test.txt (content: 11 bytes)"
        );
        // No path at all -> raw serialized arguments.
        assert_eq!(
            full_tool_args(TOOL_WRITE_FILE, &json!({"content": "x"})),
            "{\"content\":\"x\"}"
        );
    }

    // ---- long-argument truncation ----------------------------------------

    #[test]
    fn tool_args_truncates_long_run_command_preview() {
        let cmd = "echo ".to_string() + &"x".repeat(80);
        let compact = preview_tool_args(TOOL_RUN_COMMAND, &json!({ "command": cmd }));
        assert!(compact.ends_with('…'));
        assert_eq!(
            compact.chars().count(),
            COMMAND_PREVIEW_BUDGET - CLIP_SLACK + 1
        );
        let decorated = preview_tool_call(TOOL_RUN_COMMAND, &json!({ "command": cmd }));
        assert!(decorated.starts_with("run_command("));
        assert!(decorated.ends_with("…)"));
        assert_eq!(
            decorated.chars().count(),
            "run_command(".chars().count() + DECORATED_BUDGET - CLIP_SLACK + 1 + 1
        );
    }

    #[test]
    fn tool_args_truncates_long_unknown_tool_args() {
        let long = "a".repeat(60);
        let args = json!({ "k": long });
        let compact = preview_tool_args("custom_tool", &args);
        assert!(compact.ends_with('…'));
        assert_eq!(
            compact.chars().count(),
            GENERIC_PREVIEW_BUDGET - CLIP_SLACK + 1
        );
        let decorated = preview_tool_call("custom", &json!({ "arg": long }));
        assert!(decorated.starts_with("custom("));
        assert!(decorated.ends_with("…)"));
    }

    #[test]
    fn tool_args_truncation_never_splits_multi_byte_chars() {
        // Regression for the historical panic: a 3-byte `—` straddled byte index 27
        // of the serialized JSON and byte index 57 of the command string.
        let val = json!({ "k": format!("{}—rest_of_string", "a".repeat(24)) });
        let res = preview_tool_args("custom_tool", &val);
        assert!(res.ends_with('…'));
        assert_eq!(res.chars().count(), GENERIC_PREVIEW_BUDGET - CLIP_SLACK + 1);
        assert!(!res.contains('\u{fffd}'));

        // Compact budget (40 chars): the clip lands inside the 3-byte `—`.
        let cmd = format!("{}—cargo test", "b".repeat(36));
        let compact = preview_tool_args(TOOL_RUN_COMMAND, &json!({ "command": cmd.clone() }));
        assert_eq!(
            compact,
            format!(
                "{}{ELLIPSIS}",
                truncate_chars(&cmd, COMMAND_PREVIEW_BUDGET - CLIP_SLACK)
            )
        );
        assert!(compact.ends_with("—…"));
        assert_eq!(compact.chars().next_back(), Some('…'));

        // Decorated budget (60 chars): mirrors the UI render, clipped mid-word.
        let long_cmd = format!("{}—cargo check", "c".repeat(56));
        let decorated = preview_tool_call(TOOL_RUN_COMMAND, &json!({ "command": long_cmd }));
        assert!(decorated.starts_with("run_command("));
        assert!(decorated.ends_with("…)"));
        assert!(decorated.contains('—'));
        assert!(!decorated.contains('\u{fffd}'));

        // Swedish payloads in the decorated fallback must survive clipping.
        let swedish = json!({ "k": "åäö".repeat(40) });
        let clipped = preview_tool_call("custom", &swedish);
        assert!(clipped.ends_with("…)"));
        assert_eq!(
            clipped.chars().count(),
            "custom(".chars().count() + DECORATED_BUDGET - CLIP_SLACK + 1 + 1
        );
        assert!(!clipped.contains('\u{fffd}'));
    }

    // ---- remaining per-tool renders kept from both original copies --------

    #[test]
    fn tool_args_delegate_task_renders_agent_and_task_id() {
        let full = json!({ "agent_name": "debugger", "task_id": "t-042", "prompt": "fix it" });
        assert_eq!(
            preview_tool_call(TOOL_DELEGATE_TASK, &full),
            "delegate_task(agent: debugger, task_id: t-042)"
        );
        assert_eq!(
            full_tool_args(TOOL_DELEGATE_TASK, &full),
            "agent=debugger, task_id=t-042, prompt=\"fix it\""
        );
        // The compact runner preview has no delegate branch: raw JSON, clipped.
        let compact = preview_tool_args(TOOL_DELEGATE_TASK, &full);
        assert!(compact.starts_with('{'));
        assert!(compact.ends_with('…'));
        // Missing agent_name falls back to the schema default.
        assert_eq!(
            preview_tool_call(TOOL_DELEGATE_TASK, &json!({ "task_id": "t-1" })),
            "delegate_task(agent: specialist, task_id: t-1)"
        );
    }

    #[test]
    fn tool_args_create_plan_reports_markdown_size() {
        let args = json!({ "plan_markdown": "# Plan\n- step" });
        assert_eq!(
            preview_tool_call(TOOL_CREATE_PLAN, &args),
            "create_plan(plan_markdown: 13 chars)"
        );
        // Byte-counted payload size (inherited verbatim from the original UI
        // render): `åäö` is 3 characters but 6 bytes.
        let swe = json!({ "plan_markdown": "åäö" });
        assert_eq!(
            preview_tool_call(TOOL_CREATE_PLAN, &swe),
            "create_plan(plan_markdown: 6 chars)"
        );
        // Missing payload reports zero.
        assert_eq!(
            preview_tool_call(TOOL_CREATE_PLAN, &json!({})),
            "create_plan(plan_markdown: 0 chars)"
        );
    }

    #[test]
    fn tool_args_sleep_accepts_every_duration_spelling() {
        for key in ["seconds", "duration", "duration_seconds"] {
            let args = json!({ key: 12 });
            assert_eq!(preview_tool_args(TOOL_SLEEP, &args), "12s");
            assert_eq!(full_tool_args(TOOL_SLEEP, &args), "seconds=12");
            let as_text = json!({ key: "7" });
            assert_eq!(preview_tool_args(TOOL_SLEEP, &as_text), "7s");
        }
        assert_eq!(
            full_tool_args(
                TOOL_SLEEP,
                &json!({ "seconds": 3, "reason": "wait for build" })
            ),
            "seconds=3, reason=\"wait for build\""
        );
        // The decorated UI render keeps the raw JSON (as before).
        assert_eq!(
            preview_tool_call(TOOL_SLEEP, &json!({ "seconds": 3 })),
            "sleep({\"seconds\":3})"
        );
    }

    // ---- sleep-argument extraction owner (dedup gate t-044, sweep C) ------

    /// The single owner's contract: key precedence, every JSON number shape,
    /// the fallback default and one clamp pair.
    #[test]
    fn tool_args_sleep_owner_resolves_every_key_and_shape() {
        let cases: &[(&str, u64)] = &[
            // key precedence: `seconds` wins over the legacy aliases
            ("{\"seconds\":4,\"duration\":9}", 4),
            ("{\"duration\":9}", 9),
            ("{\"duration_seconds\":8}", 8),
            // missing / null / wrong type -> the shared default
            ("{}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":null}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":true}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":{}}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":[1]}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":\"later\"}", SLEEP_DEFAULT_SECS),
            // string-encoded integers were accepted by every copy
            ("{\"seconds\":\"5\"}", 5),
            ("{\"seconds\":\"-3\"}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":\"5s\"}", SLEEP_DEFAULT_SECS),
            // integers, clamped
            ("{\"seconds\":0}", SLEEP_MIN_SECS),
            ("{\"seconds\":1}", 1),
            ("{\"seconds\":-3}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":300}", SLEEP_MAX_SECS),
            ("{\"seconds\":301}", SLEEP_MAX_SECS),
            ("{\"seconds\":18446744073709551615}", SLEEP_MAX_SECS),
            // floats inside the tool's own range truncate toward zero; integral
            // ones inside the range parse exactly
            ("{\"seconds\":6.0}", 6),
            ("{\"seconds\":2.5}", 2),
            ("{\"seconds\":299.9}", 299),
            // floats outside it stay unusable -> the default, exactly like every
            // pre-existing copy (they were never parsed at all)
            ("{\"seconds\":1e9}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":300.9}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":300.0}", SLEEP_MAX_SECS),
            ("{\"seconds\":0.5}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":-2.5}", SLEEP_DEFAULT_SECS),
            ("{\"seconds\":-0.0}", SLEEP_DEFAULT_SECS),
        ];
        for (raw, expected) in cases {
            let args: Value = serde_json::from_str(raw).expect("valid fixture JSON");
            assert_eq!(
                sleep_duration_secs(&args),
                *expected,
                "unexpected duration for {raw}"
            );
        }
        // A non-object argument (truncated XML rescue payloads) cannot carry a
        // duration key: the default is returned rather than a panic. The scalar
        // value is irrelevant to the assertion, so it is spelled from the shared
        // table (`TOOL_SLEEP`) instead of a raw tool-name literal (t-069).
        for args in [Value::Null, json!(TOOL_SLEEP), json!(42), json!([])] {
            assert_eq!(sleep_duration_secs(&args), SLEEP_DEFAULT_SECS);
        }
    }

    /// The default/clamp pair exists exactly once in the codebase, here.
    #[test]
    fn tool_args_sleep_owner_owns_the_only_default_and_clamp_pair() {
        assert_eq!(SLEEP_DEFAULT_SECS, 5);
        assert_eq!(SLEEP_MIN_SECS, 1);
        assert_eq!(SLEEP_MAX_SECS, 300);
        // NOTE: the *command* timeout has its own bounds in `config.rs`
        // (`MIN/MAX_COMMAND_TIMEOUT_SECS`, also 1..=300). They are a different
        // knob and are deliberately not wired to the sleep bounds — changing
        // one must never move the other.
        // Every resolved duration sits inside the single clamp pair.
        for args in [
            json!({}),
            json!({"seconds": 0}),
            json!({"seconds": -7}),
            json!({"seconds": u64::MAX}),
            json!({"seconds": "99999"}),
            json!({"seconds": 1e300}),
            json!({"seconds": f64::MAX}),
        ] {
            let secs = sleep_duration_secs(&args);
            assert!(
                (SLEEP_MIN_SECS..=SLEEP_MAX_SECS).contains(&secs),
                "{args} -> {secs}"
            );
        }
    }

    /// The preview no longer shows a duration the executor would never wait:
    /// it renders the owner's resolved (defaulted + clamped) value.
    #[test]
    fn tool_args_sleep_preview_renders_the_resolved_duration() {
        assert_eq!(
            preview_tool_args(TOOL_SLEEP, &json!({"seconds": 999})),
            "300s"
        );
        assert_eq!(preview_tool_args(TOOL_SLEEP, &json!({"seconds": -3})), "5s");
        assert_eq!(
            preview_tool_args(TOOL_SLEEP, &json!({"seconds": 2.5})),
            "2s"
        );
        assert_eq!(
            full_tool_args(TOOL_SLEEP, &json!({"seconds": 0})),
            "seconds=1"
        );
        assert_eq!(
            full_tool_args(TOOL_SLEEP, &json!({"duration": "7", "reason": "build"})),
            "seconds=7, reason=\"build\""
        );
    }

    #[test]
    fn tool_args_leave_verdict_renders_verdict_and_comments() {
        let args = json!({ "verdict": "APPROVED", "comments": "looks good" });
        assert_eq!(
            full_tool_args(crate::tool_names::TOOL_LEAVE_VERDICT, &args),
            "verdict=APPROVED, comments=\"looks good\""
        );
        let aliases = json!({ "status": "REJECTED", "feedback": "nope" });
        assert_eq!(
            full_tool_args(crate::tool_names::TOOL_LEAVE_VERDICT, &aliases),
            "verdict=REJECTED, comments=\"nope\""
        );
    }

    #[test]
    fn tool_args_clip_helper_fits_and_clips() {
        assert_eq!(clip("abc", 10), "abc");
        assert_eq!(clip("abc", 3), "abc");
        assert_eq!(clip("abcd", 3), "…"); // budget minus slack saturates to 0
        assert_eq!(clip("abcdefgh", 6), "abc…");
    }
}
