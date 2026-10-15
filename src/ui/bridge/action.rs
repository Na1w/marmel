//! Typed routing of per-subtask steer actions on the UI-bridge side.
//!
//! The subtask action vocabulary (`ForwardNotice` | `Cancel` | `DelegateTask` | `Sleep`) is
//! owned by [`crate::orchestrator::steer`], and so is the **one** normalizer for it
//! ([`normalize_steer_subtask_action`]). The bridge never compares the raw JSON `action`
//! string: every action branch goes through the helpers below, which delegate to that
//! normalizer. There is deliberately no second normalizer here and no action vocabulary
//! spelled out as string literals — the typed constants are aliases of the orchestrator's own
//! [`SteerSubtaskAction`] variants, so UI and orchestrator cannot drift apart and equivalent
//! spellings (`Cancel`, `cancel_task`, `Abort Task`, `Sleep`, `crate::tool_names::TOOL_SLEEP`, …)
//! always take the same branch in both layers.
//!
//! A spelling outside the vocabulary is an explicit rejection: the normalizer logs it and
//! returns [`SteerSubtaskAction::Unknown`], which matches none of the bridge's branches.
//!
//! ## Two vocabularies, two owners (t-069)
//! The **subtask-action** vocabulary (`Cancel`, `abort`, `new task`, …) is steer-owned and
//! never consults the tool table. The **tool-alias** vocabulary (the `sleep` family:
//! `wait`, `sleeptask`, `waitseconds`, `pause`, the `terminal__` spelling, …) is owned by
//! [`crate::tool_names::TOOL_ALIAS_TABLE`] and reaches this module only through the
//! orchestrator's normalizer, which asks that table
//! ([`crate::tool_names::is_sleep_tool_grammar_spelling`]). This file spells neither
//! vocabulary out — the guard test below builds its needles from the table at runtime —
//! so a tool alias added to the table is accepted here automatically and a subtask action
//! added to the table would be a bug the same test surfaces.

use crate::orchestrator::SteerDecision;
use crate::orchestrator::SteerSubtaskDecision;
use crate::orchestrator::steer::normalize_steer_subtask_action;

/// The orchestrator's own action type, re-exported (not redefined) so the bridge can name the
/// action it routes to without spelling out the vocabulary.
pub(crate) use crate::orchestrator::steer::SteerSubtaskAction;

/// Post the steering notice to the running worker and await its reply.
pub(crate) const ACTION_FORWARD_NOTICE: SteerSubtaskAction = SteerSubtaskAction::ForwardNotice;
/// Cancel the running worker/subtask (fires its cancellation token).
pub(crate) const ACTION_CANCEL: SteerSubtaskAction = SteerSubtaskAction::Cancel;
/// Delegate a new ad-hoc subtask.
pub(crate) const ACTION_DELEGATE_TASK: SteerSubtaskAction = SteerSubtaskAction::DelegateTask;
/// Sleep for `sleep_seconds`.
pub(crate) const ACTION_SLEEP: SteerSubtaskAction = SteerSubtaskAction::Sleep;

/// Route one raw subtask `action` through the single shared normalizer.
pub(crate) fn route(subtask: &SteerSubtaskDecision) -> SteerSubtaskAction {
    normalize_steer_subtask_action(&subtask.action, &subtask.tool_call_id)
}

/// Route every subtask of `decision` exactly once — one subtask means one normalizer call and
/// therefore at most one rejection log line, however many branches read the decision.
pub(crate) fn routed(
    decision: Option<&SteerDecision>,
) -> Vec<(&SteerSubtaskDecision, SteerSubtaskAction)> {
    decision
        .into_iter()
        .flat_map(|d| d.subtasks.iter())
        .map(|st| (st, route(st)))
        .collect()
}

/// `true` when any subtask of `decision` routes to `action`.
pub(crate) fn any(decision: Option<&SteerDecision>, action: SteerSubtaskAction) -> bool {
    decision
        .into_iter()
        .flat_map(|d| d.subtasks.iter())
        .any(|st| route(st) == action)
}

/// The first subtask of `decision` that routes to `action`.
pub(crate) fn first(
    decision: Option<&SteerDecision>,
    action: SteerSubtaskAction,
) -> Option<&SteerSubtaskDecision> {
    decision
        .into_iter()
        .flat_map(|d| d.subtasks.iter())
        .find(|st| route(st) == action)
}

/// The worker a forwarded notice is addressed to: the agent name when the arbitrator supplied
/// one, otherwise the subtask's tool-call id.
pub(crate) fn notice_target(subtask: &SteerSubtaskDecision) -> &str {
    subtask
        .agent_name
        .as_deref()
        .unwrap_or(&subtask.tool_call_id)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn subtask(action: &str) -> SteerSubtaskDecision {
        SteerSubtaskDecision {
            tool_call_id: "tc-t069".to_string(),
            action: action.to_string(),
            message: None,
            agent_name: None,
            prompt: None,
            sleep_seconds: None,
        }
    }

    /// (t-069 item 2, test (b)) Source guard: this module keeps **no** tool-alias
    /// vocabulary of its own. The needles are built at runtime from the shared table
    /// (every alias row and every canonical spelling), so a spelling added tomorrow is
    /// covered the moment the table knows it — and this file cannot satisfy its own
    /// needles, because it never quotes a tool name.
    #[test]
    fn bridge_action_module_retains_no_tool_alias_literals() {
        let src = include_str!("action.rs");
        let production = src
            .split(&["#[cfg(", "test)", "]"].concat())
            .next()
            .expect("action.rs contains the test attribute");

        let mut needles: Vec<String> = crate::tool_names::TOOL_ALIAS_TABLE
            .iter()
            .map(|(alias, _)| format!("\"{alias}\""))
            .collect();
        for canonical in crate::tool_names::CANONICAL_TOOL_NAMES {
            needles.push(format!("\"{canonical}\""));
        }
        assert!(
            needles.len() > 30,
            "the alias-table needles must cover the whole vocabulary (got {})",
            needles.len()
        );

        for needle in &needles {
            assert!(
                !production.contains(needle.as_str()),
                "src/ui/bridge/action.rs still hard-codes the tool-name literal {needle} — the \
                 tool-alias vocabulary belongs to crate::tool_names::TOOL_ALIAS_TABLE"
            );
        }

        // Delegation, not a second normalizer: the only routing path is the shared
        // orchestrator normalizer, and this module defines no `normalize_*` of its own.
        assert!(
            production.contains("normalize_steer_subtask_action("),
            "the bridge must delegate every raw action spelling to the steer normalizer"
        );
        assert!(
            !production.contains("fn normalize_"),
            "the bridge must not grow its own action normalizer"
        );
        assert!(
            !production.contains(".parse::<") && !production.contains("FromStr"),
            "the bridge must not parse the raw action string itself"
        );
    }

    /// The two vocabularies stay separate (t-069 item 2): the tool-alias part comes
    /// from the table **through the steer normalizer**, and the bridge refuses a tool
    /// alias that is not also a subtask action.
    #[test]
    fn bridge_routes_tool_aliases_through_the_shared_table_not_its_own_list() {
        // (1) Every sleep-family spelling the shared table knows routes to the Sleep
        // branch here exactly as it does in the orchestrator.
        for spelling in crate::tool_names::tool_spellings_for(crate::tool_names::TERMINAL_SLEEP) {
            let bridged = route(&subtask(spelling));
            assert_eq!(
                bridged,
                normalize_steer_subtask_action(spelling, "tc-t069"),
                "bridge and orchestrator must route '{spelling}' identically"
            );
            assert_eq!(
                bridged, ACTION_SLEEP,
                "the table's sleep alias '{spelling}' must take the Sleep branch"
            );
        }

        // (2) Subtask-action spellings that are **not** tool aliases still route, which
        // is what keeps the two vocabularies from being merged into one.
        for action_spelling in [
            "Cancel Task",
            "abort",
            "terminate task",
            "new task",
            "send notice",
            "Forward To Worker",
        ] {
            assert!(
                crate::tool_names::normalize_tool_alias(action_spelling).is_none(),
                "'{action_spelling}' must stay outside the tool-alias vocabulary"
            );
            assert!(
                route(&subtask(action_spelling)).is_known(),
                "'{action_spelling}' is a subtask action and must route"
            );
        }

        // (3) The bridge does not accept the alias table wholesale: a tool alias that
        // is not an action stays an explicit rejection, exactly like the orchestrator's
        // verdict for the same string.
        for tool_alias in ["view_file", "bash", "edit_file", "pty__list", "save_file"] {
            assert!(
                crate::tool_names::normalize_tool_alias(tool_alias).is_some(),
                "'{tool_alias}' must really be a tool alias for this pin to mean anything"
            );
            assert_eq!(
                route(&subtask(tool_alias)),
                SteerSubtaskAction::Unknown,
                "a tool alias that is not a subtask action must stay rejected in the bridge"
            );
        }
    }
}
