//! View model for the rebuilt TUI (spec §3 pane inventory, §4 view modes and
//! focus model).
//!
//! This is a *view* model only: `TuiRenderer` remains the single owner of all
//! state (ARCH §3, spec §0). [`UiView::from_renderer`] snapshots the renderer
//! once per frame, so the render path never mutates renderer state and the
//! session loop is untouched.
//!
//! Which parts of the snapshot are state and which are derived is the whole
//! point of this module:
//!
//! * **Real, mutable UI state** — [`ViewMode`], [`FocusPane`], the follow flag
//!   and the overlay surface — is stored on the renderer as
//!   `TuiRenderer::view_mode` / `focus` / `follow` / `overlay` / `help_open` /
//!   `confirm_open`, built once in `TuiRenderer::new()` and moved only by the
//!   `pub(crate)` transition helpers in `mod.rs` (`set_view_mode`, `set_focus`,
//!   `cycle_focus_pane`, `set_follow`, `open_help`, `show_confirm`,
//!   `close_overlay`). It is never re-guessed from what another widget happens
//!   to be showing: a mode, a focus, a follow flag or an overlay the operator
//!   set stays exactly as set (spec §4.1, §4.2, §G7, §8).
//! * **Derived per frame** — [`PlanProgress`], which is a pure function of the
//!   plan text the plan pane draws (spec §3, acceptance criterion 2), and the
//!   armed-abort window, which is a wall-clock property of `confirm_abort`.

use super::{FocusedPanel, SubagentDetail, TuiRenderer};
use crate::plan_parse::{CheckboxState, parse_tasks};

/// The four view modes of spec §4.1 (M1–M4). It is stored UI state
/// (`TuiRenderer::view_mode`), not a reading of the pane toggles: the boot value
/// is the mode the boot pane inventory describes (M1 — both side panes closed),
/// and only `TuiRenderer::set_view_mode` moves it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// M1 — Transcript fills the body; Plan/Workers absent.
    Transcript,
    /// M2 — Plan left (55 %), Transcript right (45 %): the leading/trailing pair of
    /// the single slot rule, on the §2.3 `55/45` pair.
    Plan,
    /// M3 — Workers left (55 %), Transcript right (45 %). No Plan pane, so the slot
    /// rule has nothing to place in this mode.
    Workers,
    /// M4 — Plan left in the wide column (60 %), Transcript over Workers in the side
    /// column (40 %). The Plan holds the leading slot here exactly as it does in M2,
    /// so it never changes side between the two modes.
    Split,
}

impl ViewMode {
    /// The chip text the header renders for this mode (spec §2.2).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Transcript => "M1 Transcript",
            Self::Plan => "M2 Plan",
            Self::Workers => "M3 Workers",
            Self::Split => "M4 Split",
        }
    }

    /// Whether this mode draws the Plan pane (spec §4.3 "Present panes").
    pub const fn plan_pane(self) -> bool {
        matches!(self, Self::Plan | Self::Split)
    }

    /// Whether this mode draws the Workers pane (spec §4.3 "Present panes").
    pub const fn workers_pane(self) -> bool {
        matches!(self, Self::Workers | Self::Split)
    }

    /// The mode whose present panes are exactly this pair. This is the one place
    /// the mode and the two pane toggles are related, and it is a total function:
    /// every pair names a mode, so a pane toggle can never be a silent no-op
    /// (spec §4.1, G5 — the hole at `events.rs:598-609` / `render.rs:368`).
    pub const fn for_panes(plan_visible: bool, workers_visible: bool) -> Self {
        match (plan_visible, workers_visible) {
            (true, true) => Self::Split,
            (true, false) => Self::Plan,
            (false, true) => Self::Workers,
            (false, false) => Self::Transcript,
        }
    }

    /// Whether `pane` is on screen in this mode. §2.4.1 makes an absent pane
    /// "never split, never drawn, never hit-tested", so focus may never sit on
    /// one: this is what `TuiRenderer::cycle_focus_pane` skips and what a mode
    /// change uses to repair a focus the new mode would orphan (G6).
    pub const fn presents(self, pane: FocusPane) -> bool {
        match pane {
            // The composer is drawn in every mode (§2.1) and the transcript is
            // present in all four modes (§4.3).
            FocusPane::Input | FocusPane::Transcript => true,
            FocusPane::Plan => self.plan_pane(),
            FocusPane::Workers => self.workers_pane(),
        }
    }

    /// The persisted token for this mode: the one member of
    /// `session::format::VIEW_MODES` that names it, spelled from the mode itself
    /// rather than re-derived from the two pane toggles (spec §9.4 D2, §10 item
    /// 18). The toggles are still written alongside it, and they are what a
    /// pre-D2 bundle — which has no mode token at all — restores from.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Transcript => "transcript",
            Self::Plan => "plan",
            Self::Workers => "workers",
            Self::Split => "split",
        }
    }

    /// The inverse of [`ViewMode::tag`], total over `VIEW_MODES`. `None` is a
    /// token outside the set, which the loader refuses (U-chk) rather than
    /// resolves to the boot mode.
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "transcript" => Some(Self::Transcript),
            "plan" => Some(Self::Plan),
            "workers" => Some(Self::Workers),
            "split" => Some(Self::Split),
            _ => None,
        }
    }
}

/// Pane focus (spec §4.2), replacing the legacy `FocusedPanel`. Focus is drawn
/// as a `●` badge in the pane title **and** the focus border colour (G6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FocusPane {
    /// The composer. Boot focus: printable keys always reach it (spec §4.4).
    Input,
    /// The transcript pane.
    Transcript,
    /// The plan / task-overview pane.
    Plan,
    /// The worker activity pane.
    Workers,
}

impl FocusPane {
    /// The pane title spelled once (spec §7.3 titles).
    pub const fn label(self) -> &'static str {
        match self {
            Self::Input => "Input",
            Self::Transcript => "Transcript",
            Self::Plan => "Execution Plan",
            Self::Workers => "Workers",
        }
    }

    /// The `Tab` cycle of spec §4.2: `Input → Transcript → (Plan) → (Workers) →
    /// Input`. `TuiRenderer::cycle_focus_pane` walks this ring and skips the
    /// panes the current mode does not present.
    ///
    /// The ring is the §4.2 **inventory** order, not a left-to-right walk of the
    /// panes, and t-024b deliberately leaves it alone: the order is what `Shift+Tab`
    /// reverses and what the persisted focus tags were spelled against, and it was
    /// never positional — in M3 the ring already stepped Transcript → Workers while
    /// the roster sat in the left column. Which pane sits where is geometry, and
    /// [`super::layout`] is the only owner of that.
    pub const fn cycle_order() -> [FocusPane; 4] {
        [Self::Input, Self::Transcript, Self::Plan, Self::Workers]
    }

    /// The **one** predicate that answers the question every scroll and every
    /// composer key asks: does the composer hold focus, or does a body pane?
    ///
    /// It is stated here, off the real focus, so the key router in `events.rs` has
    /// exactly one way to answer it. The legacy [`FocusPane::legacy`] fold cannot
    /// do that: it spells `Input` and `Transcript` with the same `Chat` token, so
    /// it cannot tell a composer key from a transcript key, which is what made the
    /// composer's history walk and the panes' scrolling fight over `Up`/`Down`.
    /// The rule the predicate carries (spec §4.2, §4.4):
    ///
    /// * the composer owns a key **iff** `Input` holds focus;
    /// * a pane viewport owns a key **iff** some other pane holds focus.
    ///
    /// Printables, paste, `Backspace`/`Delete` and the `Ctrl+Z/Y/J` chords are the
    /// deliberate exception to that rule — they reach the composer in every focus
    /// so typing is never starved (§4.4) — and they are the reason the predicate is
    /// named for *focus* rather than for *typing*.
    pub const fn composer_has_focus(self) -> bool {
        matches!(self, Self::Input)
    }

    /// The legacy [`FocusedPanel`] this pane projects onto. `Input` and
    /// `Transcript` share the `Chat` spelling — the legacy enum never had a
    /// composer focus (spec §4.2) — and this mirror is what the pre-rebuild fields
    /// of `mod.rs` (the `legacy_view` snapshot and the bridge that adopts it) still
    /// carry. It is **not** the persistence
    /// route any more: since plumbing delta D2 (spec §9.4) the durable bundle
    /// spells the pane through [`FocusPane::tag`], so nothing stored ever goes
    /// through this fold. Key routing does **not** read it either: it asks
    /// [`FocusPane::composer_has_focus`] and then scrolls the pane itself, because
    /// this fold cannot tell the composer from the transcript.
    pub const fn legacy(self) -> FocusedPanel {
        match self {
            Self::Input | Self::Transcript => FocusedPanel::Chat,
            Self::Plan => FocusedPanel::Plan,
            Self::Workers => FocusedPanel::Subagents,
        }
    }

    /// The inverse of [`FocusPane::legacy`], for the pre-rebuild field only. It
    /// cannot recover which of `Input`/`Transcript` the `Chat` spelling came from
    /// — that is exactly why it is no longer on the persistence path — and it
    /// lands on `Transcript`, the body pane the old `Chat` focus named.
    pub const fn from_legacy(panel: FocusedPanel) -> Self {
        match panel {
            FocusedPanel::Chat => Self::Transcript,
            FocusedPanel::Plan => Self::Plan,
            FocusedPanel::Subagents => Self::Workers,
        }
    }

    /// The persisted token for this pane: the one member of
    /// `session::format::FOCUSED_PANELS` that names it, and the value
    /// `ViewState::focused_panel` carries (spec §9.4 D2, §10 item 18). One token
    /// per pane, so a capture → restore cycle loses no focus and the composer is
    /// no longer folded into the transcript. `"chat"` is kept as the transcript's
    /// token because every bundle written before D2 holds that spelling and it
    /// must keep meaning the same pane.
    pub const fn tag(self) -> &'static str {
        match self {
            Self::Input => "input",
            Self::Transcript => "chat",
            Self::Plan => "plan",
            Self::Workers => "subagents",
        }
    }

    /// The inverse of [`FocusPane::tag`], total over `FOCUSED_PANELS`: each token
    /// names exactly one pane, including `"input"`. `None` is a token outside the
    /// set, which the loader refuses (U-chk) rather than repairs — and
    /// [`TuiRenderer::restore_panes`] keeps the focus it already holds for one.
    pub fn from_tag(tag: &str) -> Option<Self> {
        match tag {
            "input" => Some(Self::Input),
            "chat" => Some(Self::Transcript),
            "plan" => Some(Self::Plan),
            "subagents" => Some(Self::Workers),
            _ => None,
        }
    }
}

/// The modal surface currently on screen (spec §3 help/confirm overlays, §4.4,
/// §8.5). Closed by `Esc`; opening it never arms abort.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OverlayState {
    /// No overlay: panes own the frame.
    None,
    /// The `?`/`F1` keymap + command legend (G2).
    Help,
    /// A destructive action awaiting `y`/`n` (G3), and the armed-abort prompt.
    Confirm,
}

impl OverlayState {
    /// Map the two open surfaces onto the one surface that is on screen.
    ///
    /// `confirm_open` is `TuiRenderer::confirm_open` — a destructive action
    /// awaiting a `y`/`n`, which the armed-abort prompt also occupies while the
    /// 3 s window is live (§8.5) — and `help_open` is the `?`/`F1` legend
    /// toggle. A confirm always wins: §5 honours `y`/`n` only inside the confirm
    /// overlay, and §7.3 draws exactly one overlay per frame. This is the single
    /// mapping the transition helpers of `mod.rs` (`open_help`, `show_confirm`,
    /// `close_overlay`) feed, so the surface on screen is always a function of
    /// the two stored flags and never of what a pane is drawing.
    pub const fn for_surface(confirm_open: bool, help_open: bool) -> Self {
        if confirm_open {
            Self::Confirm
        } else if help_open {
            Self::Help
        } else {
            Self::None
        }
    }

    /// Whether any modal surface covers the body (spec §7.3: overlays draw last).
    pub const fn is_open(self) -> bool {
        !matches!(self, Self::None)
    }
}

/// Plan progress derived from the same `plan_content` string the plan pane draws,
/// through the existing plan grammar (`crate::plan_parse`) — spec §3 (Header row)
/// and acceptance criterion 2: `done/total` is exactly the `CheckboxState::Checked`
/// / total split of that string.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanProgress {
    /// Task lines checked off.
    pub done: usize,
    /// All task lines in the plan.
    pub total: usize,
    /// The task currently in flight, if one is.
    pub active_task: Option<String>,
}

impl PlanProgress {
    /// Derive progress for `plan_content` (the renderer's cached plan text).
    ///
    /// The active task follows the rule already used by the plan pane
    /// (`render.rs:512-538`): the explicit `active_plan_task`, else the task id
    /// of any running worker row.
    pub fn from_plan(
        plan_content: &str,
        active_plan_task: Option<&str>,
        subagents: &[SubagentDetail],
    ) -> Self {
        let tasks = parse_tasks(plan_content);
        let total = tasks.len();
        let done = tasks
            .iter()
            .filter(|task| task.checkbox == CheckboxState::Checked)
            .count();
        let active_task = active_plan_task.map(str::to_string).or_else(|| {
            subagents
                .iter()
                .find(|row| row.is_active)
                .and_then(|row| row.task_id.clone())
        });
        Self {
            done,
            total,
            active_task,
        }
    }

    /// Task lines still open.
    pub const fn pending(&self) -> usize {
        self.total.saturating_sub(self.done)
    }

    /// Whether the plan text carries at least one task line (the empty/absent
    /// plan sentinel parses to zero tasks).
    pub const fn has_tasks(&self) -> bool {
        self.total > 0
    }

    /// The `3/7 done` chip — never dropped, at any width (G1).
    pub fn progress_label(&self) -> String {
        format!("{}/{} done", self.done, self.total)
    }
}

/// Scroll offsets of the scrollable panes, re-clamped on resize (spec §2.4.4).
///
/// Every entry is a plain offset — the position the pane last drew, or the one the
/// scroll keys left. **No follow decision is encoded in an offset here.** Each pane
/// is handed its own follow request as a flag next to the offset
/// (`TranscriptData::follow`, `PlanData::auto_scroll`,
/// `ActivityData::autoscroll`), because "stay on the newest line" is only ever
/// answerable by the pass that counts the body: a tail resolved in the state layer
/// would be an estimate of the tail, and an estimate is precisely the second geometry
/// that let a scroll key and a frame disagree about one viewport (audit defects
/// 11/12/15).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollState {
    /// Transcript pane offset (renderer's `chat_scroll`).
    pub transcript: u16,
    /// Plan pane offset (renderer's `plan_scroll`).
    pub plan: u16,
    /// Worker detail offset (renderer's `subagent_scroll`). Whether that offset is
    /// even used is the pane's call, made from `subagent_autoscroll`.
    pub workers: u16,
}

/// The per-frame view model handed to `components::layout::compute_layout` and
/// to every pane component.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UiView {
    /// M1–M4 (spec §4.1).
    pub mode: ViewMode,
    /// Focused pane (spec §4.2).
    pub focus: FocusPane,
    /// Transcript follow (`LIVE` / `↓ N new`, spec G7).
    pub follow: bool,
    /// Overlay currently on screen (spec §3).
    pub overlay: OverlayState,
    /// Whether the Plan pane is toggled on (`Ctrl+P`, `/plan`).
    pub plan_visible: bool,
    /// Whether the Workers pane is toggled on (`Ctrl+A`, `/workers`).
    pub workers_visible: bool,
    /// Whether thinking blocks are displayed (`Ctrl+T`, `/thought`).
    pub show_thought: bool,
    /// Whether an abort is armed inside the 3 s window (spec §8.5).
    pub abort_armed: bool,
    /// Draft line count driving `input_h` (spec §2.1).
    pub input_lines: usize,
    /// Scroll offsets of the scrollable panes.
    pub scroll: ScrollState,
    /// Derived plan progress rendered in every frame (G1).
    pub plan: PlanProgress,
}

impl UiView {
    /// Snapshot the renderer into the view model. Read-only: no renderer state
    /// is written here (spec §0 — the render path keeps its existing cadence and
    /// the session loop keeps driving it).
    ///
    /// The mode, the focused pane, the follow flag and the overlay surface are
    /// read straight out of the renderer's own state fields — they are the
    /// operator's, set once at boot and moved by the transition helpers of
    /// `mod.rs`, and they are never inferred from a pane toggle or another
    /// widget (spec §4.1, §4.2, §8.6). Only [`PlanProgress`] and the armed-abort
    /// window are computed here, because both really are functions of other
    /// state: the plan text and the abort-arm timestamp.
    pub fn from_renderer(renderer: &TuiRenderer) -> Self {
        let plan = PlanProgress::from_plan(
            &renderer.plan_content,
            renderer.active_plan_task.as_deref(),
            &renderer.subagents,
        );
        Self {
            mode: renderer.view_mode,
            focus: renderer.focus,
            follow: renderer.follow,
            overlay: renderer.overlay,
            abort_armed: renderer.is_abort_armed(),
            input_lines: renderer.textarea.lines().len().max(1),
            scroll: ScrollState {
                transcript: renderer.chat_scroll,
                plan: renderer.plan_scroll,
                // A verbatim mirror, in every pane. The follow flags are carried to
                // the panes beside these offsets — `TranscriptData::follow`,
                // `PlanData::auto_scroll`, `ActivityData::autoscroll` — and each pane
                // resolves "the newest line" against the body it counts, so the view
                // model never rewrites an offset into a tail it cannot measure
                // (audit defect 15, where `subagent_autoscroll` was answered only by
                // a pass outside the pane, and defects 11/12, where that pass and the
                // pane then drew two different geometries).
                workers: renderer.subagent_scroll,
            },
            plan_visible: renderer.show_plan_panel,
            workers_visible: renderer.show_subagent_panel,
            show_thought: renderer.show_thought,
            plan,
        }
    }

    /// Whether `pane` holds focus right now (drives the `●` badge and the border
    /// colour, spec §7.3).
    pub fn focused(&self, pane: FocusPane) -> bool {
        self.focus == pane
    }

    /// Whether the composer is the typing target (spec §4.4).
    pub fn composer_active(&self) -> bool {
        // Only a modal overlay takes the keyboard away; `Esc` closes it without
        // arming abort (§4.4).
        !self.overlay.is_open()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One roster row whose detail body is long enough that "the tail" is a place
    /// the pane has to scroll to reach.
    fn row_with_a_long_log() -> SubagentDetail {
        SubagentDetail {
            name: "coder_specialist-t-004".to_string(),
            logs: (0..40)
                .map(|index| format!("- log line {index:02}"))
                .collect(),
            is_active: true,
            ..SubagentDetail::default()
        }
    }

    /// Defects 11/12/15, the state half: the view model mirrors offsets and carries
    /// follow decisions **beside** them, never inside them. `ScrollState` never
    /// substitutes a tail for a stored offset, because the tail of a pane is only
    /// ever known by the pass that counts that pane's body — an offset the state
    /// layer invented would be a second, rival geometry, which is exactly what let a
    /// scroll key and a frame disagree about one viewport. Each pane is handed its
    /// follow request as a flag next to the offset:
    /// `TranscriptData::follow`, `PlanData::auto_scroll`,
    /// `ActivityData::autoscroll`.
    #[test]
    fn the_scroll_state_mirrors_offsets_and_encodes_no_follow_decision_in_them() {
        let mut renderer = TuiRenderer::new();
        renderer.subagents.push(row_with_a_long_log());
        renderer.chat_scroll = 7;
        renderer.plan_scroll = 2;
        renderer.subagent_scroll = 3;
        renderer.set_follow(true);
        renderer.subagent_autoscroll = true;
        renderer.plan_auto_scroll = true;

        let view = UiView::from_renderer(&renderer);
        assert_eq!(view.scroll.transcript, 7, "the transcript's own offset");
        assert_eq!(view.scroll.plan, 2, "the plan's own offset");
        assert_eq!(
            view.scroll.workers, 3,
            "the detail offset the scroll keys left, verbatim, however long the log is"
        );
        assert!(
            view.follow,
            "transcript follow crosses to the pane as the flag it is"
        );
        assert_eq!(
            usize::from(renderer.subagent_scroll),
            3,
            "and no stored offset was rewritten on the way in"
        );
    }

    /// The same mirror with every follow flag off: the view model is a reading, not a
    /// decision, so an operator's parked offset is handed to the pane untouched and
    /// the pane cannot be handed a tail it was never asked for.
    #[test]
    fn a_detail_scrolled_off_its_tail_is_handed_over_as_that_offset() {
        let mut renderer = TuiRenderer::new();
        renderer.subagents.push(row_with_a_long_log());
        renderer.subagent_autoscroll = false;
        renderer.set_follow(false);
        renderer.subagent_scroll = 3;

        let view = UiView::from_renderer(&renderer);
        assert_eq!(view.scroll.workers, 3, "the operator's offset, verbatim");
        assert!(
            usize::from(view.scroll.workers) < usize::from(renderer.worker_detail_tail()),
            "and it is still short of the tail the key path measures"
        );
    }

    /// Nothing in the state layer follows on its own: a follow flag standing over an
    /// offset that is not the tail moves no viewport, and the follow is realised only
    /// by the frame that measures the pane.
    #[test]
    fn a_follow_flag_over_an_offset_that_is_not_the_measured_tail_moves_nothing() {
        let mut renderer = TuiRenderer::new();
        renderer.subagents.push(row_with_a_long_log());
        let tail = usize::from(renderer.worker_detail_tail());
        assert!(tail > 1, "the fixture has a tail to be short of");
        renderer.subagent_scroll = (tail - 1) as u16;
        assert!(renderer.subagent_autoscroll);

        let view = UiView::from_renderer(&renderer);
        assert_eq!(
            usize::from(view.scroll.workers),
            tail - 1,
            "one line short of the measured tail, and it stays there"
        );
    }

    /// With no row on the roster there is no detail half to follow, so the entry is
    /// simply the stored field — which is what the view model has always been for a
    /// pane that is not on screen.
    #[test]
    fn an_empty_roster_mirrors_the_detail_offset_instead_of_inventing_one() {
        let mut renderer = TuiRenderer::new();
        renderer.subagent_scroll = 11;
        assert!(renderer.subagent_autoscroll);
        assert!(renderer.subagents.is_empty(), "no row, so no detail half");
        assert_eq!(UiView::from_renderer(&renderer).scroll.workers, 11);
    }
}
