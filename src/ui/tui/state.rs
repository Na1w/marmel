//! View model for the rebuilt TUI (spec §3 pane inventory, §4 view modes and
//! focus model).
//!
//! This is a *view* model only: `TuiRenderer` remains the single owner of all
//! state (ARCH §3, spec §0). Every value here is derived per frame from the
//! renderer by [`UiView::from_renderer`], so the render path never mutates
//! renderer state and the session loop is untouched.

use super::{FocusedPanel, SubagentDetail, TuiRenderer};
use crate::plan_parse::{CheckboxState, parse_tasks};

/// The four view modes of spec §4.1 (M1–M4). Default is [`ViewMode::Split`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ViewMode {
    /// M1 — transcript fills the body; Plan/Workers absent.
    Transcript,
    /// M2 — Plan left (55 %), Transcript right (45 %).
    Plan,
    /// M3 — Workers left (55 %), Transcript right (45 %).
    Workers,
    /// M4 — Transcript left (60 %), Plan over Workers in the side column (40 %).
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
    /// Map the surfaces that can currently be open onto the overlay model.
    ///
    /// `confirm_open` is the armed destructive action awaiting a second key
    /// press; `help_open` is the `?`/`F1` legend. Written as one mapping so the
    /// key handlers of t-010 only have to feed it, not invent it.
    pub const fn for_surface(confirm_open: bool, help_open: bool) -> Self {
        if help_open {
            Self::Help
        } else if confirm_open {
            Self::Confirm
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
        let active_task = active_plan_task
            .map(str::to_string)
            .or_else(|| {
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
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ScrollState {
    /// Transcript pane offset (renderer's `chat_scroll`).
    pub transcript: u16,
    /// Plan pane offset (renderer's `plan_scroll`).
    pub plan: u16,
    /// Worker detail offset (renderer's `subagent_scroll`).
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
    /// The boot state of §4.1/§4.2 (M4 Split, composer focus, follow on, closed
    /// overlay) is what a fresh renderer reports: `Split` when both side panes
    /// are toggled on, `FocusPane::Input` as the focus the body panes cycle
    /// through, and `chat_auto_scroll` as follow.
    pub fn from_renderer(renderer: &TuiRenderer) -> Self {
        let plan = PlanProgress::from_plan(
            &renderer.plan_content,
            renderer.active_plan_task.as_deref(),
            &renderer.subagents,
        );
        // Legacy `FocusedPanel` tracks only the three body panes; the composer is
        // the typing target in every mode (spec §4.4) and keeps the boot focus of
        // §4.2 until `Tab` cycling (t-010) moves the focus explicitly.
        let focus = match renderer.focused_panel {
            FocusedPanel::Chat => FocusPane::Transcript,
            FocusedPanel::Plan => FocusPane::Plan,
            FocusedPanel::Subagents => FocusPane::Workers,
        };
        // The renderer owns no `view_mode` field yet (t-010 adds it), so the mode
        // is derived from the two pane toggles: neither pane is M1, one pane is
        // M2/M3, both are the M4 default (spec §4.3).
        let mode = match (renderer.show_plan_panel, renderer.show_subagent_panel) {
            (true, true) => ViewMode::Split,
            (true, false) => ViewMode::Plan,
            (false, true) => ViewMode::Workers,
            (false, false) => ViewMode::Transcript,
        };
        Self {
            mode,
            focus,
            follow: renderer.chat_auto_scroll,
            // The `?`/F1 legend and the `y`/`n` confirm requests are wired by
            // t-010, so the only surface observable today is the armed abort,
            // which §8.5 draws as the status row-2 prompt, never as a modal.
            overlay: OverlayState::for_surface(renderer.is_abort_armed(), false),
            abort_armed: renderer.is_abort_armed(),
            input_lines: renderer.textarea.lines().len().max(1),
            scroll: ScrollState {
                transcript: renderer.chat_scroll,
                plan: renderer.plan_scroll,
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
