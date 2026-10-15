//! Per-worker section types (Design A §3.8, §3.9).

use serde::{Deserialize, Serialize};

use crate::agents::Agent;

use super::messages::MessageRecord;
use super::restore::NonRestorableNotice;
use super::{SectionHeader, TimePoint};

/// On-disk shape of `workers/<key>.json`. One file per worker, one worker per
/// file — never an array, so a single corrupt worker can be isolated (A D6).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerFile {
    pub header: SectionHeader,
    pub section: WorkerSection,
}

/// Everything needed to re-insert an identical entry into the live `WORKERS` map
/// and to rebuild the worker's conversation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerSection {
    /// The **exact** registry key, including any `#…` disambiguator. This field,
    /// not the filename, is the authority; the filename is an escaped handle
    /// (A §5.2). Restore inserts under this key and never re-registers.
    pub worker_key: String,
    /// The value drawn from `WORKER_ID_SEQ` when this worker registered,
    /// recorded so `{agent}-wN` keys and disambiguators reproduce exactly.
    pub worker_id_seq_value: u64,
    /// Agent role. Reuses the crate's role vocabulary (`agents::Agent`,
    /// `#[serde(rename_all = "snake_case")]`) — role names are a shared
    /// vocabulary, not a session format, so A D1 does not fork them.
    pub role: Agent,
    /// Normalized task id binding (`Option`; `None` for task-less workers, whose
    /// key is `{agent}-wN`).
    pub task_id: Option<String>,

    /// Lifecycle class used by cancel/notice/`has_active_workers()` routing.
    pub status: WorkerStatus,
    /// The live free-form label, verbatim (`"In Progress"`, `"Revising"`,
    /// `"Approved"`, `"Aborted"`, `"Failed"`, `"Completed"`) — the bytes the
    /// subagent list renders.
    pub status_label: String,

    /// Clocks (A D5). `started` mirrors `started_at`/`started_wall`; `completed`
    /// mirrors `completed_at`/`completed_wall`; `last_activity` mirrors
    /// `SubagentDetail::last_activity_at`.
    pub started: TimePoint,
    pub completed: Option<TimePoint>,
    pub last_activity: Option<TimePoint>,
    /// Accumulated working time as an explicit number of seconds, so it never
    /// has to be computed as a difference of two re-anchored instants (D5 T3).
    pub worked_secs: f64,

    /// Completed-ring identity (`WorkerState::completed_seq`); `None` ⇔ active.
    /// Restore keeps the stored value and must not re-draw it.
    pub completed_seq: Option<u64>,

    /// Token counters: live `context_tokens` and the post-completion
    /// `last_tokens`.
    pub context_tokens: usize,
    pub last_tokens: usize,
    /// Budget/turn counters.
    pub counters: WorkerCounters,

    /// The delegation seed — required to re-advertise the same task.
    pub brief: String,
    pub snippets: Vec<String>,
    pub image_urls: Vec<String>,
    pub audio_urls: Vec<String>,
    pub recursion_granted: bool,

    /// The worker's **full** conversation, in order (A §5.5). Not the seed.
    pub history: Vec<MessageRecord>,
    /// Provenance of `history`: whether it was mirrored from the live engine or
    /// is only the seed. A `SeedOnly` history is reported, never presented as an
    /// exact restoration (A D6 "loudly").
    pub history_source: HistorySource,

    /// Where the worker's tool-call pairing stood at the capture instant.
    pub dangling: DanglingToolCallState,

    /// Populated iff `status == Unrecoverable` (A D6's only tolerated partial).
    pub unrecoverable_reason: Option<String>,
    /// Projections that lost a capability for this worker (A D5): its
    /// cancellation token, its task handle, its live PTY, its guard, its queued
    /// notices.
    pub non_restorable: Vec<NonRestorableNotice>,
}

/// Lifecycle class. Deliberately orthogonal to `status_label`: the label is
/// display bytes, this is control state. `Unrecoverable` is produced only by the
/// loader (A D6) and never written by a healthy capture.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WorkerStatus {
    /// Active in the pre-crash process (`completed_seq == None`).
    InProgress,
    Completed,
    Failed,
    Aborted,
    /// A live status string this build does not recognize. Preserved verbatim in
    /// `status_label`; never coerced into one of the four known classes.
    Unknown,
    /// Loader verdict: this worker's file failed verification and was the only
    /// corrupt member (A D6). Its history is gone; it is reported loudly and its
    /// pane identity is kept so the UI still shows *that it existed*.
    Unrecoverable,
}

/// Budget/turn counters, carried verbatim from `ActiveWorkerInfo`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkerCounters {
    pub implementation_turns: usize,
    pub validation_rounds: usize,
    pub latest_validator_feedback: Option<String>,
}

/// Provenance of a `WorkerSection::history` vector.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HistorySource {
    /// Mirrored out of the live engine turn by turn (A §5.5). Exactly restorable.
    LiveMirror,
    /// Only the seed was recoverable (a worker that never reached the first
    /// mirror point). Reported, never silently upgraded to a "history".
    SeedOnly,
    /// History intentionally not captured (e.g. a worker already completed and
    /// evicted from the mirror ring). `history` is then empty.
    NotCaptured,
}

/// The assistant `tool_calls` ↔ `role:"tool"` pairing state at the capture
/// instant — the invariant `manager::context` documents in both directions.
/// Recorded so a restore can *report* what it healed instead of discovering a
/// provider 400 at the first request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum DanglingToolCallState {
    /// Every `tool_calls` entry has its result, and no orphan results exist.
    Balanced,
    /// An assistant turn has `tool_calls` entries with no result yet — the exact
    /// shape left by persisting the assistant before the round's dispatch.
    /// `dispatched` says whether the tool began (its result may exist elsewhere,
    /// e.g. in a PTY whose process survived the kill).
    Dangling { calls: Vec<DanglingCall> },
    /// `role:"tool"` results whose parent is gone. The prune pass removes them
    /// at load; the ids are reported.
    OrphanedResults { tool_call_ids: Vec<String> },
    /// Capture already applied the `ABORTED_TOOL_RESULT` placeholder, recording
    /// which ids it synthesized. The loader does not re-synthesize these.
    Healed { aborted_tool_call_ids: Vec<String> },
}

/// One unpaired `tool_calls` entry, carried verbatim so the report can show the
/// call that never completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DanglingCall {
    pub tool_call_id: String,
    pub function_name: String,
    /// Verbatim arguments.
    pub arguments: String,
    pub dispatched: bool,
}

impl DanglingToolCallState {
    /// The pairing state a capture *observes* for `messages`, computed with the
    /// same vocabulary the repair pass uses (no second pairing rule — A D1).
    ///
    /// Used by the capture seam (t-009) and asserted on by the loader's N6
    /// cross-check, which reports a disagreement and never reconciles it.
    pub fn observe(messages: &[MessageRecord]) -> Self {
        let mut parents: Vec<(String, String, String)> = Vec::new();
        let mut results: Vec<String> = Vec::new();
        for m in messages {
            match m {
                MessageRecord::Assistant { tool_calls, .. } => {
                    for tc in tool_calls {
                        parents.push((
                            tc.id.clone(),
                            tc.function.name.clone(),
                            tc.function.arguments.clone(),
                        ));
                    }
                }
                MessageRecord::Tool { tool_call_id, .. } => results.push(tool_call_id.clone()),
                MessageRecord::System { .. } | MessageRecord::User { .. } => {}
            }
        }
        let parent_ids: Vec<&String> = parents.iter().map(|(id, ..)| id).collect();

        let orphans: Vec<String> = results
            .iter()
            .filter(|id| !parent_ids.contains(id))
            .cloned()
            .collect();
        if !orphans.is_empty() {
            return DanglingToolCallState::OrphanedResults {
                tool_call_ids: orphans,
            };
        }

        let dangling: Vec<DanglingCall> = parents
            .iter()
            .filter(|(id, ..)| !results.contains(id))
            .map(|(id, name, arguments)| DanglingCall {
                tool_call_id: id.clone(),
                function_name: name.clone(),
                arguments: arguments.clone(),
                dispatched: false,
            })
            .collect();
        if !dangling.is_empty() {
            return DanglingToolCallState::Dangling { calls: dangling };
        }

        let aborted: Vec<String> = messages
            .iter()
            .filter_map(|m| match m {
                MessageRecord::Tool {
                    tool_call_id,
                    content,
                } if content.as_str() == super::ABORTED_TOOL_RESULT => Some(tool_call_id.clone()),
                _ => None,
            })
            .collect();
        if !aborted.is_empty() {
            return DanglingToolCallState::Healed {
                aborted_tool_call_ids: aborted,
            };
        }
        DanglingToolCallState::Balanced
    }
}
