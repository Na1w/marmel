//! The UI section (Design A §3.6 + §6): pane text is the byte authority (D4),
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

/// Two authorities, exactly one of them for display (A D4): `chat_messages` for
/// the chat pane, `subagent_panels` for the subagent panes. `journal` exists for
/// steering correlation only.
///
/// Deliberately **not** `Eq`: `SubagentPanel::worked_duration_secs` is an `f64`
/// (A §11.3 finding 5).
///
/// `Default` is derived, and that is only sound because [`ViewState`]'s own
/// `Default` is hand-written: a derived default `ViewState` would carry
/// `focused_panel: ""`, which U-chk (A §6.5) refuses, and then
/// `UiSection::default()` would not be a loadable bundle.
///
/// **t-020 pane-coverage extension:** the plan panel, the two streaming
/// partials, the agent identity, the token counters and the abort-arming witness
/// were appended as `#[serde(default)]` fields, so the version gate stays where
/// A §3.1 put it: `SCHEMA_VERSION` is **unchanged**, because every new field has
/// a serde default and an old `ui.json` therefore still parses — and each
/// default is the value `Default` would have produced, so an old bundle keeps
/// passing `structural_violations()` (U-chk refuses, it never invents).
/// Compatibility is one-directional and honestly so: this struct is still
/// `deny_unknown_fields`, so a section written by THIS binary is refused by an
/// older one as a version mismatch rather than silently half-read
/// (`session/format/mod.rs:10`).
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UiSection {
    // ---- display authority (A §3.6) ----
    /// The chat pane, one entry per pane line, **exactly as displayed** (1:1 with
    /// `TuiRenderer::messages`). Includes the prefixes the pane spells
    /// (`"User: "`, `"[Tool Call] "`, …) and the local `[CLI]` lines the semantic
    /// journal never records. Restore SHALL assign this vector and SHALL NOT
    /// regenerate any entry from a journal record (U1–U3).
    pub chat_messages: Vec<String>,
    /// Exact status-line text, so the status bar is byte-identical instead of
    /// resetting to `"Ready"`.
    pub status_line: String,

    // ---- subagent panes (display authority for those panes, A §6.3) ----
    pub subagent_panels: Vec<SubagentPanel>,

    // ---- semantic journal mirror (A §6.4) ----
    pub journal: Vec<UiJournalRecord>,

    // ---- composer (gap 5) ----
    pub composer: ComposerState,

    // ---- view state: selection + scroll (gap 6) ----
    pub view: ViewState,

    // ---- budget counters (witness only, A §6.5) ----
    pub budgets: BudgetWitness,

    // ---- plan panel (gap 10, t-020) ----
    //
    // A §6.5 declared these "not persisted by decision" because the plan FILE
    // (`.marmel/execution_plan.md`) is the authority for a fresh process
    // (`tui/mod.rs:417-424` re-reads it at `init`). That decision is superseded
    // for the *pane text*: the mission is "the exact text in every UI pane", and
    // between `init` and the first plan-file poll the pane would otherwise show
    // `"No active execution plan."` over a screen that said something else.
    // The plan file stays the authority for a live session; these fields are
    // what the pane actually displayed at capture instant.
    /// The plan pane's text verbatim (`TuiRenderer::plan_content`, `tui/mod.rs:49`).
    #[serde(default)]
    pub plan_content: String,
    /// The auto-open/auto-close latch (`tui/mod.rs:78`): it decides whether the
    /// elapsed-time figure in the plan border is drawn, so it is pane state too.
    #[serde(default)]
    pub had_active_plan: bool,
    /// Task the plan box was flagging as in flight (`tui/mod.rs:135`). A §6.2 U5
    /// called this derived; it is persisted here because it is *drawn*, and it is
    /// restored as display state only (the live loop recomputes it on the next
    /// tool call, which is the pre-existing behaviour).
    #[serde(default)]
    pub active_plan_task: Option<String>,
    /// `plan_start_time` / `plan_completed_time` (`tui/mod.rs:167`, `:169`) as the
    /// D5 `TimePoint` projection: the plan elapsed timer is an `Instant` pair and
    /// can only survive as a projection.
    #[serde(default)]
    pub plan_start: Option<TimePoint>,
    #[serde(default)]
    pub plan_completed: Option<TimePoint>,

    // ---- streaming partials (gap 11, t-020) ----
    //
    // A §10.2 said these are "deliberately not persisted" because persisting a
    // half-turn would fabricate a finished message. They are persisted here
    // because they are **pane text** — the thinking/content region of the chat
    // pane shows them live (`tui/render.rs:204-210`, `:450-456`) and losing them
    // loses visible bytes. They stay display-only: restore SHALL NOT resume the
    // provider request (§10.2) and SHALL NOT invent a finished transcript record;
    // the text simply sits in the streaming region where the screen left it.
    /// Partial thinking streamed for the turn in flight (`tui/mod.rs:45`).
    #[serde(default)]
    pub current_thought: String,
    /// Partial answer text streamed for the turn in flight (`tui/mod.rs:47`).
    #[serde(default)]
    pub current_content: String,

    // ---- agent / notice identity (gap 12, t-020) ----
    /// Which agent the status line was crediting (`tui/mod.rs:142`; `"Manager"`
    /// when none). `None` means "old bundle, field absent" — the renderer then
    /// keeps whatever it derived at boot instead of inventing `"Manager"`.
    #[serde(default)]
    pub active_agent: Option<String>,
    /// Half-collected steer sentence waiting for its newline (`tui/mod.rs:97`).
    /// It is a pane-boundary buffer: dropping it silently loses the first words
    /// of the next notice line.
    #[serde(default)]
    pub steer_sentence_buffer: Option<String>,

    // ---- token counters (witness only, t-020) ----
    //
    // Same rule as `budgets` (A §6.5): a persisted counter is compared and
    // reported, never adopted, because the live session's counters belong to the
    // live streams. `tui/mod.rs:434-444` derives them from `Event::TokensIn`/
    // `Event::TokensOut`, so adopting stored numbers would double-count or erase a
    // running total.
    /// Estimated prompt tokens for the session (`tui/mod.rs:153`).
    #[serde(default)]
    pub tokens_in: u64,
    /// Estimated completion tokens for the session (`tui/mod.rs:155`).
    #[serde(default)]
    pub tokens_out: u64,
    /// Context-window size in tokens of the orchestrator turn (`tui/mod.rs:157`).
    #[serde(default)]
    pub orchestrator_context_tokens: u64,

    // ---- abort arming (display-only witness, t-020) ----
    //
    // A §6.5 keeps `confirm_abort` / `abort_armed_at` out of the bundle ("a
    // 3-second armed window has no meaning after a restart"), and §10's closed
    // inventory has no rule that re-arms it. Both hold: the fields below record
    // that the previous screen was showing "Abort armed. Press ESC again to
    // abort.", and restore SHALL NOT re-arm the live renderer — re-arming would
    // let one keypress abort a session the user never asked to abort.
    /// Was the confirm-abort window open at capture instant (`tui/mod.rs:58`,
    /// `:60`, and `TuiRenderer::is_abort_armed` at `tui/mod.rs:344`).
    #[serde(default)]
    pub abort_armed: bool,
    /// When it was armed, as the D5 projection — the pair lets a reader state how
    /// long the window had left. It is never converted back into an `Instant`
    /// that could fire an abort.
    #[serde(default)]
    pub abort_armed_at: Option<TimePoint>,
}

/// Durable projection of one `SubagentDetail`, field-for-field, with the two
/// `Instant`s and the `Duration` replaced by the D5 projection.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentPanel {
    /// The specialist role id (`coder`, `researcher`, …) — the join key the live
    /// merge uses.
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
    /// `worked_duration: Duration` as an explicit number of seconds (D5 T3 —
    /// never a difference of two re-anchored instants).
    pub worked_duration_secs: f64,
    pub started_at: Option<TimePoint>,
    pub last_activity_at: Option<TimePoint>,
    /// Per-subagent **turn** thinking (t-020): the mirror of the renderer's
    /// `subagent_turn_thinking` map (`tui/mod.rs:163`) for this panel's `name`.
    /// `None` = the map had no entry, which is a real distinction: the detail
    /// pane falls back to `thinking` in that case (`tui/render.rs:864-878`), so
    /// an empty string and an absent entry draw different text.
    #[serde(default)]
    pub turn_thinking: Option<String>,
    /// The mirror of `subagent_is_thinking` (`tui/mod.rs:165`): whether this
    /// subagent was streaming reasoning tokens at capture instant. The spinner
    /// and the "thinking …/budget" bar in the detail pane are driven by it, so
    /// without it the pane comes back in the wrong state. `None` = no entry.
    #[serde(default)]
    pub is_thinking: Option<bool>,
}

/// Persistence mirror of `ui::transcript::UiRecord` — the same 8 variants, the
/// same payload types, the same `#[serde(tag = "type")]` layout. Declared here
/// (A D1) because `UiTranscript` MUST NOT become a persistence type again (D2).
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
    /// Whether the plan box was on screen (t-020; `tui/mod.rs:76`). It sits here
    /// because its sibling `show_subagent_panel` does: it is a viewer toggle
    /// (`events.rs:179` flips it), not plan content.
    #[serde(default)]
    pub show_plan_panel: bool,
    pub chat_auto_scroll: bool,
    pub subagent_autoscroll: bool,
    pub plan_auto_scroll: bool,
}

/// Hand-written rather than derived on purpose: `#[derive(Default)]` would give
/// `focused_panel: ""`, which U-chk (A §6.5) refuses. The default here mirrors
/// the renderer's own initial state — `focused_panel: FocusedPanel::Chat`
/// (`src/ui/tui/mod.rs:192`), `show_thought`/`show_subagent_panel`/
/// `show_plan_panel` false (`:195-198`) — so a hand-built `UiSection::default()`
/// is a *loadable* bundle. Every field added later (t-020) keeps that property:
/// its serde default is the same value written here.
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
/// reported, never adopted (A §4 R-PIN-6 pattern).
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
    /// U-chk (A §6.5): the structural preconditions of `ui.json`, checked before
    /// any assignment. These are **preconditions, not repairs** — the loader
    /// never "fixes" a caret or a selection; every violation is a
    /// `Refused { HeaderManifestDisagreement, member: "ui.json" }`.
    ///
    /// Returns one sentence per violation, quoting the offending value.
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
        // t-020: the plan timer and the abort-arming witness are `TimePoint`s
        // too, so they get the same A §5.6 sanity rule (negative / NaN /
        // infinite monotonic offsets are a refusal, never defaulted to now).
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
