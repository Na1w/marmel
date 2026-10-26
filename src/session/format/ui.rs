//! The UI section: pane text is the byte authority,
//! the semantic journal is a steering mirror only.

use serde::{Deserialize, Serialize};

use super::SectionHeader;
use super::TimePoint;

/// On-disk shape of `ui.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiFile {
    pub header: SectionHeader,
    pub section: UiSection,
}

/// Two authorities, exactly one of them for display: `chat_messages` for the chat
/// pane, `subagent_panels` for the subagent panes. `journal` is a steering mirror
/// only. Deliberately **not** `Eq`: `SubagentPanel::worked_duration_secs` is an
/// `f64`. `Default` is derived, and that is only sound because [`ViewState`]'s own
/// `Default` is hand-written: a derived default `ViewState` would carry
/// `focused_panel: ""`, which U-chk refuses, and then `UiSection::default()` would
/// not be a loadable bundle. New pane fields arrive as `#[serde(default)]` whose
/// default is exactly what `Default` produces, so `SCHEMA_VERSION` is unchanged and
/// an old `ui.json` still parses; `deny_unknown_fields` makes this binary's output a
/// version mismatch for an older binary rather than a silent half-read.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    // ---- display authority ----
    /// The chat pane, one entry per pane line, **exactly as displayed** (1:1 with
    /// `TuiRenderer::messages`). Includes the prefixes the pane spells
    /// (`"User: "`, `"[Tool Call] "`, …) and the local `[CLI]` lines the semantic
    /// journal never records. Restore SHALL assign this vector and SHALL NOT
    /// regenerate any entry from a journal record.
    pub chat_messages: Vec<String>,
    /// Exact status-line text, so the status bar is byte-identical instead of
    /// resetting to `"Ready"`.
    pub status_line: String,

    // ---- subagent panes (display authority for those panes) ----
    pub subagent_panels: Vec<SubagentPanel>,

    // ---- semantic journal mirror ----
    pub journal: Vec<UiJournalRecord>,

    // ---- composer ----
    pub composer: ComposerState,

    // ---- view state: selection + scroll ----
    pub view: ViewState,

    // ---- budget counters (witness only) ----
    pub budgets: BudgetWitness,

    // ---- plan panel ----
    //
    // The plan file (`.marmel/execution_plan.md`) stays the authority for a live
    // session; these fields are what the plan *pane* displayed at capture
    // instant. Without them the pane shows `"No active execution plan."` between
    // boot and the first plan-file poll.
    /// The plan pane's text verbatim (`TuiRenderer::plan_content`).
    #[serde(default)]
    pub plan_content: String,
    /// The auto-open/auto-close latch: it decides whether the
    /// elapsed-time figure in the plan border is drawn, so it is pane state too.
    #[serde(default)]
    pub had_active_plan: bool,
    /// Task the plan box was flagging as in flight. It is persisted here because
    /// it is *drawn*, and it is restored as display state only: the live loop
    /// recomputes it on the next tool call, as it already does.
    #[serde(default)]
    pub active_plan_task: Option<String>,
    /// `plan_start_time` / `plan_completed_time` as the
    /// `TimePoint` projection: the plan elapsed timer is an `Instant` pair and
    /// can only survive as a projection.
    #[serde(default)]
    pub plan_start: Option<TimePoint>,
    #[serde(default)]
    pub plan_completed: Option<TimePoint>,

    // ---- streaming partials ----
    //
    // They are **pane text**: the chat pane's thinking/content region shows them
    // live, and losing them loses visible bytes. Restore SHALL NOT resume the
    // provider request and SHALL NOT invent a finished transcript record — the
    // text sits in the streaming region where the screen left it.
    /// Partial thinking streamed for the turn in flight.
    #[serde(default)]
    pub current_thought: String,
    /// Partial answer text streamed for the turn in flight.
    #[serde(default)]
    pub current_content: String,

    // ---- agent / notice identity ----
    /// Which agent the status line was crediting (`"Manager"`
    /// when none). `None` means "old bundle, field absent" — the renderer then
    /// keeps whatever it derived at boot instead of inventing `"Manager"`.
    #[serde(default)]
    pub active_agent: Option<String>,
    /// Half-collected steer sentence waiting for its newline.
    /// It is a pane-boundary buffer: dropping it silently loses the first words
    /// of the next notice line.
    #[serde(default)]
    pub steer_sentence_buffer: Option<String>,

    // ---- token counters (witness only) ----
    //
    // Same rule as `budgets`: a persisted counter is compared and reported, never
    // adopted, because the live session's counters belong to the live streams,
    // which derive them from `Event::TokensIn`/`Event::TokensOut`; adopting
    // stored numbers would double-count or erase a running total.
    /// Estimated prompt tokens for the session.
    #[serde(default)]
    pub tokens_in: u64,
    /// Estimated completion tokens for the session.
    #[serde(default)]
    pub tokens_out: u64,
    /// Context-window size in tokens of the orchestrator turn.
    #[serde(default)]
    pub orchestrator_context_tokens: u64,

    // ---- abort arming (display-only witness) ----
    //
    // A 3-second armed window has no meaning after a restart and nothing here
    // re-arms it: the fields below record only that the previous screen showed
    // "Abort armed. Press ESC again to abort.", and restore SHALL NOT re-arm the
    // live renderer — re-arming would let one keypress abort a session the user
    // never asked to abort.
    /// Was the confirm-abort window open at capture instant
    /// (`TuiRenderer::is_abort_armed`).
    #[serde(default)]
    pub abort_armed: bool,
    /// When it was armed, as a `TimePoint` projection — the pair lets a reader
    /// state how long the window had left. It is never converted back into an
    /// `Instant` that could fire an abort.
    #[serde(default)]
    pub abort_armed_at: Option<TimePoint>,
}

/// Durable projection of one `SubagentDetail`, field-for-field, with the two
/// `Instant`s and the `Duration` replaced by a `TimePoint` projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentPanel {
    /// The **effective registry key** of the worker this panel projects — the same
    /// string `WorkerEntry::key` carries (`{agent}-{task}`, `{agent}-wN` or the
    /// `{base}#N` form a duplicate registration takes), and therefore the join key
    /// the live merge and every registry lookup use. It is never a display name
    /// derived from the agent enum.
    pub name: String,
    pub task_id: Option<String>,
    /// Never rendered, but required to re-advertise the same subagent.
    pub prompt: String,
    /// Pane text (logs / thinking / content) — the **only** recoverable copy of
    /// the subagent panes.
    pub logs: Vec<String>,
    pub thinking: String,
    pub content: String,
    pub is_active: bool,
    pub context_tokens: usize,
    /// `worked_duration: Duration` as an explicit number of seconds — never a
    /// difference of two re-anchored instants.
    pub worked_duration_secs: f64,
    pub started_at: Option<TimePoint>,
    pub last_activity_at: Option<TimePoint>,
    /// Per-subagent **turn** thinking: the mirror of the renderer's
    /// `subagent_turn_thinking` map for this panel's `name`.
    /// `None` = the map had no entry, which is a real distinction: the detail
    /// pane falls back to `thinking` in that case, so
    /// an empty string and an absent entry draw different text.
    #[serde(default)]
    pub turn_thinking: Option<String>,
    /// The mirror of `subagent_is_thinking`: whether this
    /// subagent was streaming reasoning tokens at capture instant. The spinner
    /// and the "thinking …/budget" bar in the detail pane are driven by it, so
    /// without it the pane comes back in the wrong state. `None` = no entry.
    #[serde(default)]
    pub is_thinking: Option<bool>,

    // ---- queue / arbitration-pause state (design §9.4 D2) ----
    //
    // Both arrive as `#[serde(default)]` with a default that is exactly what
    // `Default` produces, so an `ui.json` written before these fields existed still
    // parses and `SCHEMA_VERSION` is unchanged. Nothing else may change here:
    // `deny_unknown_fields` stays, and no existing field is renamed or retyped.
    /// Concurrency-slot queue state — the durable twin of
    /// [`crate::ui::SubagentDetail::queue`]. `None` = the row was not waiting for a
    /// backend slot at capture instant.
    #[serde(default)]
    pub queue: Option<QueuePanel>,
    /// Whether this worker's stream was held by steering arbitration at capture
    /// instant — the durable twin of [`crate::ui::SubagentDetail::paused`].
    #[serde(default)]
    pub paused: bool,
}

/// Durable projection of [`crate::ui::QueueState`]: a 1-based FIFO rank and the
/// queue depth that contains it. Kept as its own struct so the pair can be
/// `Option`-wrapped (absent = not waiting) instead of encoded as two sentinels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QueuePanel {
    pub position: usize,
    pub of: usize,
}

/// Persistence mirror of `ui::transcript::UiRecord` — the same 8 variants, the
/// same payload types, the same `#[serde(tag = "type")]` layout. Declared here
/// because `UiTranscript` is not a persistence type.
///
/// **Never display authority.** If a journal `User { text }` and a
/// `chat_messages` entry disagree, `chat_messages` wins for the screen and the
/// journal wins for steering; neither is derived from the other.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase", deny_unknown_fields)]
pub enum UiJournalRecord {
    User {
        text: String,
    },
    Assistant {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        content: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thinking: Option<String>,
    },
    SteerResponse {
        text: String,
    },
    ToolCall {
        display: String,
    },
    ToolResult {
        display: String,
    },
    TaskCompleted {
        task_id: String,
    },
    TaskFailed {
        task_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
    },
    Status {
        text: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ComposerState {
    /// Half-typed message, verbatim, embedded newlines included.
    pub input_text: String,
    /// Caret position, same unit as `TuiRenderer::cursor` (always on a char
    /// boundary).
    pub cursor: usize,
    /// Draft saved when history navigation began.
    pub input_draft: String,
    /// Command history, oldest first.
    pub history: Vec<String>,
    /// `None` = not navigating.
    pub history_index: Option<usize>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ViewState {
    pub chat_scroll: u16,
    pub plan_scroll: u16,
    pub subagent_scroll: u16,
    pub subagent_list_scroll: u16,
    pub selected_subagent_idx: usize,
    /// `"chat" | "plan" | "subagents"` — mirrors `FocusedPanel`; a value outside
    /// that set is a refusal (U-chk).
    pub focused_panel: String,
    pub show_thought: bool,
    pub show_subagent_panel: bool,
    /// Whether the plan box was on screen. It sits here
    /// because its sibling `show_subagent_panel` does: it is a viewer toggle
    /// (`events.rs` flips it), not plan content.
    #[serde(default)]
    pub show_plan_panel: bool,
    pub chat_auto_scroll: bool,
    pub subagent_autoscroll: bool,
    pub plan_auto_scroll: bool,
}

/// Hand-written rather than derived on purpose: `#[derive(Default)]` would give
/// `focused_panel: ""`, which U-chk refuses. The default here mirrors
/// the renderer's own initial state — `focused_panel: FocusedPanel::Chat`,
/// `show_thought`/`show_subagent_panel`/`show_plan_panel` all false — so a
/// hand-built `UiSection::default()` is a *loadable* bundle. Every field added
/// later keeps that property: its serde default is the same value written here.
impl Default for ViewState {
    fn default() -> Self {
        ViewState {
            chat_scroll: 0,
            plan_scroll: 0,
            subagent_scroll: 0,
            subagent_list_scroll: 0,
            selected_subagent_idx: 0,
            focused_panel: "chat".to_string(),
            show_thought: false,
            show_subagent_panel: false,
            show_plan_panel: false,
            chat_auto_scroll: false,
            subagent_autoscroll: false,
            plan_auto_scroll: false,
        }
    }
}

/// Witness-only: the budgets are owned by `Config`, so a persisted difference is
/// reported, never adopted.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BudgetWitness {
    /// Sorted map so the committed bytes are deterministic across runs.
    pub thinking_budgets: std::collections::BTreeMap<String, usize>,
    pub default_thinking_budget: usize,
}

/// The legal `ViewState::focused_panel` spellings.
pub const FOCUSED_PANELS: &[&str] = &["chat", "plan", "subagents"];

impl UiSection {
    /// U-chk: the structural state `ui.json` must be in before any assignment.
    /// These are **checks, not repairs** — the loader never "fixes" a caret or a
    /// selection; every violation is a
    /// `Refused { HeaderManifestDisagreement, member: "ui.json" }`.
    ///
    /// Returns one sentence per violation, quoting the value at fault.
    pub fn structural_violations(&self) -> Vec<String> {
        let mut out = Vec::new();

        if self.composer.cursor > self.composer.input_text.len() {
            out.push(format!(
                "composer.cursor {} is past the end of input_text ({} bytes)",
                self.composer.cursor,
                self.composer.input_text.len()
            ));
        } else if !self
            .composer
            .input_text
            .is_char_boundary(self.composer.cursor)
        {
            out.push(format!(
                "composer.cursor {} is not on a char boundary of input_text",
                self.composer.cursor
            ));
        }
        if !FOCUSED_PANELS.contains(&self.view.focused_panel.as_str()) {
            out.push(format!(
                "view.focused_panel {:?} is not one of {FOCUSED_PANELS:?}",
                self.view.focused_panel
            ));
        }
        if let Some(idx) = self.composer.history_index
            && idx >= self.composer.history.len()
        {
            out.push(format!(
                "composer.history_index {} is outside history ({} entries)",
                idx,
                self.composer.history.len()
            ));
        }
        if self.subagent_panels.is_empty() {
            if self.view.selected_subagent_idx != 0 {
                out.push(format!(
                    "view.selected_subagent_idx {} must be 0 when there are no panels",
                    self.view.selected_subagent_idx
                ));
            }
        } else if self.view.selected_subagent_idx >= self.subagent_panels.len() {
            out.push(format!(
                "view.selected_subagent_idx {} is outside subagent_panels ({} panels)",
                self.view.selected_subagent_idx,
                self.subagent_panels.len()
            ));
        }
        for panel in &self.subagent_panels {
            for (field, tp) in [
                ("started_at", &panel.started_at),
                ("last_activity_at", &panel.last_activity_at),
            ] {
                if let Some(tp) = tp
                    && !tp.is_sane()
                {
                    out.push(format!(
                        "subagent {} {field} is not a sane TimePoint: {tp:?}",
                        panel.name
                    ));
                }
            }
        }
        // The plan timer and the abort-arming witness are `TimePoint`s too, so
        // they get the same sanity rule: a negative, NaN or infinite monotonic
        // offset is a refusal, never defaulted to now.
        for (field, tp) in [
            ("plan_start", &self.plan_start),
            ("plan_completed", &self.plan_completed),
            ("abort_armed_at", &self.abort_armed_at),
        ] {
            if let Some(tp) = tp
                && !tp.is_sane()
            {
                out.push(format!("{field} is not a sane TimePoint: {tp:?}"));
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One panel with every field carrying something distinguishable.
    fn panel() -> SubagentPanel {
        SubagentPanel {
            name: "coder-t-012".to_string(),
            task_id: Some("t-012".to_string()),
            prompt: "plumb the panes".to_string(),
            logs: vec!["started task t-012".to_string()],
            thinking: String::new(),
            content: "done".to_string(),
            is_active: true,
            context_tokens: 12,
            worked_duration_secs: 3.5,
            started_at: Some(TimePoint::now()),
            last_activity_at: Some(TimePoint::now()),
            turn_thinking: None,
            is_thinking: Some(true),
            queue: Some(QueuePanel { position: 2, of: 3 }),
            paused: true,
        }
    }

    fn section_with(panel: &SubagentPanel) -> UiSection {
        UiSection {
            subagent_panels: vec![panel.clone()],
            ..UiSection::default()
        }
    }

    /// D2 round trip: the queue pair and the arbitration pause survive the durable
    /// format byte-for-byte, so a restore puts the row back in the state the screen
    /// was showing instead of downgrading it to a running row.
    #[test]
    fn the_queue_pair_and_the_pause_round_trip_through_the_durable_format() {
        // One fixture value: `TimePoint::now()` re-samples on every call, so building
        // the expectation twice would compare two different clocks.
        let fixture = panel();
        let encoded =
            serde_json::to_string(&section_with(&fixture)).expect("a section always serialises");
        let decoded: UiSection = serde_json::from_str(&encoded).expect("our own output re-reads");
        assert_eq!(decoded.subagent_panels, vec![fixture.clone()]);
        let restored = &decoded.subagent_panels[0];
        assert_eq!(
            restored.queue,
            Some(QueuePanel { position: 2, of: 3 }),
            "the rank the pane was showing is still there: {encoded}"
        );
        assert!(restored.paused, "the held stream is still marked held");
    }

    /// Backwards compatibility: a `ui.json` written **before** D2 carries neither
    /// `queue` nor `paused`. Both arrive as `#[serde(default)]`, so the older bundle
    /// loads and the row comes back without either state — which is exactly what the
    /// older binary produced, and never an invented position.
    #[test]
    fn a_panel_written_before_the_queue_and_pause_fields_still_loads() {
        let mut value = serde_json::to_value(section_with(&panel())).expect("serialisable");
        let panels = value["subagent_panels"]
            .as_array_mut()
            .expect("one panel in the fixture");
        let removed = ["queue", "paused"];
        for entry in panels.iter_mut() {
            let object = entry.as_object_mut().expect("a panel object");
            for field in removed {
                assert!(
                    object.remove(field).is_some(),
                    "the fixture must really carry {field} before it is stripped"
                );
            }
        }
        let decoded: UiSection =
            serde_json::from_value(value).expect("a pre-D2 bundle must still load");
        let restored = &decoded.subagent_panels[0];
        assert_eq!(
            restored.queue, None,
            "no rank was recorded, none is claimed"
        );
        assert!(!restored.paused, "no pause was recorded, none is claimed");
        // Everything the older binary did record still arrives unchanged.
        assert_eq!(restored.name, "coder-t-012");
        assert_eq!(restored.context_tokens, 12);
        assert_eq!(restored.logs, vec!["started task t-012".to_string()]);
        assert_eq!(restored.worked_duration_secs, 3.5);
        assert_eq!(restored.is_thinking, Some(true));
    }

    /// The D2 fields were added without loosening the contract: `deny_unknown_fields`
    /// still refuses a bundle carrying a field this binary does not know, so a
    /// newer writer is a version mismatch and never a half-read pane.
    #[test]
    fn adding_fields_with_defaults_did_not_open_the_reader_to_unknown_ones() {
        let mut value = serde_json::to_value(panel()).expect("serialisable");
        value.as_object_mut().expect("a panel object").insert(
            "written_by_a_newer_release".to_string(),
            serde_json::json!(1),
        );
        let err = serde_json::from_value::<SubagentPanel>(value)
            .expect_err("an unknown field must be refused, not ignored");
        assert!(
            err.to_string().contains("written_by_a_newer_release"),
            "the refusal names the offending field: {err}"
        );
    }

    /// `SCHEMA_VERSION` stays at 1 and `UiSection::default()` stays a loadable
    /// bundle: the new fields default to exactly what `Default` produces, so an old
    /// `ui.json` needs no migration and the default section still passes its own
    /// structural check.
    #[test]
    fn the_default_section_is_still_a_loadable_bundle_with_the_new_fields_present() {
        let encoded = serde_json::to_string(&UiSection::default()).expect("serialisable");
        let decoded: UiSection =
            serde_json::from_str(&encoded).expect("the default section re-reads");
        assert_eq!(decoded, UiSection::default());
        assert!(
            UiSection::default().structural_violations().is_empty(),
            "the default bundle must stay loadable"
        );
        // A queued, paused row adds no structural violation either: the pair is a
        // display state, and the registry — not the bundle — stays the authority that
        // moves or retires it from here on.
        let with_queue = section_with(&panel());
        assert!(
            with_queue.structural_violations().is_empty(),
            "{with_queue:?}"
        );
    }
}
