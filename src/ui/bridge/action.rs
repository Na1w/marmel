//! Typed routing of per-subtask steer actions on the UI-bridge side.
//!
//! The subtask action vocabulary (`ForwardNotice` | `Cancel` | `DelegateTask` | `Sleep`) is
//! owned by [`crate::orchestrator::steer`], and so is the **one** normalizer for it
//! ([`normalize_steer_subtask_action`]). The bridge never compares the raw JSON `action`
//! string: every action branch delegates to that normalizer. There is no second normalizer
//! and no action vocabulary spelled out as string literals — the typed constants are aliases
//! of the orchestrator's own [`SteerSubtaskAction`] variants, so UI and orchestrator cannot
//! drift apart and equivalent spellings (`Cancel`, `cancel_task`, `Abort Task`, `Sleep`,
//! `crate::tool_names::TOOL_SLEEP`, …) always take the same branch in both layers.
//!
//! A spelling outside the vocabulary is an explicit rejection: the normalizer logs it and
//! returns [`SteerSubtaskAction::Unknown`], which matches none of the bridge's branches.
//!
//! Two vocabularies, two owners: the subtask-action vocabulary (`Cancel`, `abort`, `new
//! task`, …) is steer-owned and never consults the tool table, while the tool-alias
//! vocabulary (the sleep-family spellings) is owned by
//! [`crate::tool_names::TOOL_ALIAS_TABLE`] and reaches this module only through the
//! orchestrator's normalizer, which asks that table
//! ([`crate::tool_names::is_sleep_tool_grammar_spelling`]).

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

/// The **one** worker-addressing rule the bridge uses — cancel *and* post-to-worker.
///
/// The pair `(agent, task)` is resolved against the worker registry, never against pane rows
/// and never by an OR of name fields: each non-empty argument must match the **same** entry
/// as a whole token, which is exactly the rule the registry itself applies when cancelling
/// (`orchestrator::workers::worker_matches`) and when routing a notice
/// ([`crate::orchestrator::notice::WorkerRoutingIdentity::routes`]). Both arguments empty
/// addresses nothing.
///
/// The identity is built by the single authority the orchestrator defers to,
/// [`crate::orchestrator::notice::worker_routing_identity`], with the authoritative
/// `agent_name` / `task_id` pinned off the registry entry — the same two steps
/// `workers::worker_identity` performs, so the bridge cannot grow a third identity model.
///
/// Returns the **effective registry keys** (`{agent}-{task}`, `{agent}-wN`, `{base}#N`) of the
/// matching *active* entries in registry order; those are the keys the pane rows are named by.
pub(crate) fn addressed_worker_keys(agent: Option<&str>, task: Option<&str>) -> Vec<String> {
    let clean_agent =
        crate::task_id::normalize_task_id_ref(agent.unwrap_or("")).to_ascii_lowercase();
    let clean_task = crate::task_id::normalize_task_id_ref(task.unwrap_or("")).to_ascii_lowercase();
    if clean_agent.is_empty() && clean_task.is_empty() {
        return Vec::new();
    }

    crate::orchestrator::worker_entries()
        .into_iter()
        .filter(|entry| entry.is_active)
        .filter(|entry| {
            let identity = routing_identity(entry);
            let agent_matches = clean_agent.is_empty() || identity.routes(&clean_agent);
            let task_matches = clean_task.is_empty() || identity.routes(&clean_task);
            agent_matches && task_matches
        })
        .map(|entry| entry.key)
        .collect()
}

/// Registry identity of one entry, resolved through the notice router's single authority.
fn routing_identity(
    entry: &crate::orchestrator::WorkerEntry,
) -> crate::orchestrator::notice::WorkerRoutingIdentity {
    let mut identity = crate::orchestrator::notice::worker_routing_identity(&entry.key);

    let authoritative_agent = entry.agent_name.trim();
    if !authoritative_agent.is_empty() {
        identity.agent_name = authoritative_agent.to_ascii_lowercase();
    }

    let authoritative_task = entry
        .task_id
        .as_deref()
        .map(|raw| crate::task_id::normalize_task_id_ref(raw).to_ascii_lowercase())
        .filter(|clean| !clean.is_empty());
    identity.task_id = authoritative_task.or(identity.task_id);

    identity
}

/// The worker a forwarded notice is addressed to.
///
/// The arbitrator supplies free-form tokens (`agent_name`, `tool_call_id`), and posting one
/// of those tokens verbatim is only safe when a live worker actually drains the inbox it
/// becomes. `notice_targets` therefore resolves the pair against the worker registry and
/// refuses, rather than silently filing an orphan inbox:
///
/// * the **pair** resolves through [`addressed_worker_keys`] (whole-token AND semantics,
///   the same rule the registry applies when cancelling and the notice router applies when
///   delivering);
/// * exactly one match posts under that **effective registry key** — the exact spelling the
///   runner drains its inbox with, which is what makes the worker's `reply_to_arbitrator`
///   pass the strict reply contract instead of being rejected `worker-mismatch`;
/// * several matches post **one notice per key** (never a de-facto role broadcast under a
///   raw name token);
/// * when the pair matches nothing, the agent name alone is tried, then an explicit
///   broadcast token (`worker` / `*`) is honoured as the deliberate broadcast it is;
/// * anything else is [`NoticeTargeting::Unroutable`] — the host must surface it, because
///   filing it under a token no worker drains loses the instruction and poisons the reply
///   contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoticeTargeting {
    /// Post exactly one notice **per key**, in registry order.
    Addresses(Vec<String>),
    /// Nothing may be posted. `token` is what the arbitrator addressed, kept for the
    /// visible rejection.
    Unroutable { token: String },
}

/// Resolve the addressing of one `ForwardNotice` subtask (see [`NoticeTargeting`]).
pub(crate) fn notice_targets(subtask: &SteerSubtaskDecision) -> NoticeTargeting {
    let agent = subtask.agent_name.as_deref().unwrap_or("").trim();
    let task = subtask.tool_call_id.trim();

    match addressed_worker_keys(Some(agent), Some(task)).as_slice() {
        [] => {}
        keys => return NoticeTargeting::Addresses(keys.to_vec()),
    }

    // The arbitrator echoed something that is not the registry's task id (an OpenAI-style
    // `call_…` id is the usual case). The agent half can still address one worker — but
    // only if the router really accepts that token for a live entry.
    if !agent.is_empty() {
        let by_agent = addressed_worker_keys(Some(agent), None);
        if !by_agent.is_empty() {
            return NoticeTargeting::Addresses(by_agent);
        }
    }

    // A bare broadcast token is the arbitrator deliberately addressing "whatever is
    // running"; that documented semantics stays, and it is the only non-registry token
    // this function will ever emit.
    for token in [task, agent] {
        if crate::orchestrator::notice::BROADCAST_TARGETS
            .iter()
            .any(|b| *b == token.to_ascii_lowercase().as_str())
        {
            return NoticeTargeting::Addresses(vec![token.to_string()]);
        }
    }

    NoticeTargeting::Unroutable {
        token: {
            let token = if agent.is_empty() { task } else { agent };
            if token.is_empty() {
                "the arbitrator named no worker".to_string()
            } else {
                token.to_string()
            }
        },
    }
}

/// The transcript line a completed subtask's **typed** verdict produces.
///
/// `Deliverable::marker` is the verdict the specialist actually returned
/// ([`crate::markers::MissionMarker`]) — the same value the plan layer checks a task line
/// off against — and it used to be dropped at this boundary: the bridge printed
/// `deliverable.content` and left the transcript to decide the badge by scraping that
/// prose. This projects the marker instead, in the shapes [`crate::markers`] owns
/// (`MISSION COMPLETE (t-007)`, `FAILED: <reason>`, `REPLAN REQUIRED (t-007): <reason>`),
/// so `MISSION COMPLETE` / `FAILED` / `REPLAN REQUIRED` badging is driven by data
/// (DATA §6 gap 10).
///
/// The line is deliberately **marker-led**: `transcript::verdict_of` emphasises a line
/// only when it opens with a marker, which is what keeps a prose mention of a verdict
/// word unbadged. A `REVOKED` verdict keeps coming from the assembled deliverable,
/// because revocation is applied to the marker inside that text by the runner — the
/// content is printed too, so the revocation still reaches the pane as the runner wrote
/// it.
pub(crate) fn deliverable_verdict_line(
    task_id: &str,
    deliverable: &crate::agents::Deliverable,
) -> String {
    use crate::markers::{self, MissionMarker};
    let task = task_id.trim();
    match &deliverable.marker {
        MissionMarker::Complete { .. } => {
            if task.is_empty() {
                markers::MARKER_COMPLETE.to_string()
            } else {
                markers::decorated(markers::MARKER_COMPLETE, task)
            }
        }
        MissionMarker::Failed { reason } => {
            format!("{}: {}", markers::MARKER_FAILED, reason.trim())
        }
        MissionMarker::Replan { reason } => {
            let head = if task.is_empty() {
                markers::MARKER_REPLAN.to_string()
            } else {
                markers::decorated(markers::MARKER_REPLAN, task)
            };
            format!("{head}: {}", reason.trim())
        }
    }
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

    /// Source guard: this module keeps **no** tool-alias vocabulary of its own. The
    /// needles are built at runtime from the shared table (every alias row and every
    /// canonical spelling), so a spelling added later is covered the moment the table
    /// knows it — and this file cannot satisfy its own needles, because it never quotes
    /// a tool name.
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

    /// The two vocabularies stay separate: the tool-alias part comes from the table
    /// **through the steer normalizer**, and the bridge refuses a tool alias that is
    /// not also a subtask action.
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

    #[test]
    fn a_deliverables_verdict_line_is_led_by_the_marker_the_plan_layer_acts_on() {
        use crate::agents::Deliverable;
        use crate::markers::{
            self, MARKER_COMPLETE, MissionMarker, decorated, starts_with_complete,
        };

        // Completion: the decorated marker the plan layer checks a line off against,
        // spelled by `markers` and not by this module.
        let complete = Deliverable {
            marker: MissionMarker::Complete {
                task_id: Some("t-012".to_string()),
            },
            content: "plumbed the panes\n\nMISSION COMPLETE (t-012)".to_string(),
            task_id: Some("t-012".to_string()),
        };
        let line = deliverable_verdict_line("t-012", &complete);
        assert_eq!(line, decorated(MARKER_COMPLETE, "t-012"), "{line}");
        assert!(starts_with_complete(&line), "{line}");

        // Failure: the sanctioned `FAILED: <reason>` shape, so the transcript badges
        // the line from the marker value rather than by scraping the body prose.
        let failed = Deliverable {
            marker: MissionMarker::Failed {
                reason: "tests did not run".to_string(),
            },
            content: "I could not finish".to_string(),
            task_id: Some("t-012".to_string()),
        };
        let line = deliverable_verdict_line("t-012", &failed);
        assert_eq!(line, "FAILED: tests did not run", "{line}");
        assert!(markers::has_failure_marker(&line), "{line}");

        // Replan: the strongest statement, decorated with the task it reopens.
        let replan = Deliverable {
            marker: MissionMarker::Replan {
                reason: "the plan misses the migration".to_string(),
            },
            content: "this needs a different plan".to_string(),
            task_id: Some("t-012".to_string()),
        };
        let line = deliverable_verdict_line("t-012", &replan);
        assert_eq!(
            line, "REPLAN REQUIRED (t-012): the plan misses the migration",
            "{line}"
        );
        // Marker-led on purpose: `transcript::verdict_of` badges a line only when it
        // opens with a marker, so the projected value is what drives the badge.
        assert!(line.starts_with(markers::MARKER_REPLAN), "{line}");

        // A deliverable with no task id never renders an empty `()` decoration.
        let taskless = Deliverable {
            marker: MissionMarker::Complete { task_id: None },
            content: "done".to_string(),
            task_id: None,
        };
        assert_eq!(deliverable_verdict_line("", &taskless), MARKER_COMPLETE);
    }
}
