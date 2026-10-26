//! Interactive 3-panel Ratatui terminal UI.

mod events;
pub mod formatting;
mod render;

mod components;
mod layout;
mod state;
mod theme;

pub use formatting::*;

#[cfg(test)]
mod tests;

use super::{Event, Renderer, SteerStage, SteerTrace, SubagentDetail};
use crate::config::Config;
use crate::orchestrator::OrchestratorManager;
use anyhow::Result;
use crossterm::event::{
    DisableBracketedPaste, DisableMouseCapture, EnableBracketedPaste, KeyboardEnhancementFlags,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::execute;
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use std::io;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::time::Duration;

/// Which panel currently holds focus (drives border color and navigation).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FocusedPanel {
    Chat,
    Plan,
    Subagents,
}

/// The last value the renderer itself wrote on each legacy view field.
///
/// The rebuilt view state (`view_mode` / `focus` / `follow`) is authoritative,
/// but the key path of `events.rs` still writes the pre-rebuild fields
/// (`show_plan_panel`, `show_subagent_panel`, `focused_panel`,
/// `chat_auto_scroll`) directly until t-011 routes those keys through the
/// transition helpers here. Comparing the live field against this mirror tells
/// [`TuiRenderer::adopt_legacy_view_writes`] the one thing it needs to know: did
/// somebody write behind the API? A mirror the API keeps current therefore fires
/// exactly once per legacy write, and never re-reads a mode, a focus or a follow
/// flag out of a pane toggle the operator has already moved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct LegacyViewMirror {
    /// Last seen `show_plan_panel`.
    pub(crate) plan_visible: bool,
    /// Last seen `show_subagent_panel`.
    pub(crate) workers_visible: bool,
    /// Last seen `focused_panel`.
    pub(crate) focus: FocusedPanel,
    /// Last seen `chat_auto_scroll`.
    pub(crate) follow: bool,
    /// Last seen result of [`TuiRenderer::is_abort_armed`]. Arming is edge
    /// triggered so an expiry (the 3 s window lapsing on its own) closes the
    /// Confirm overlay exactly once instead of re-opening it every frame.
    pub(crate) abort_armed: bool,
}

/// Status-line substrings that mean "a turn is in flight, keep animating".
///
/// One source of truth for the two places that must never drift apart: the
/// render-loop throttle tier in [`TuiRenderer::drain_and_throttle_flush`] (a
/// spinner-only repaint every 125 ms while nothing else is dirty) and the
/// activity indicator in [`TuiRenderer::render_status`]. If they disagree, a
/// frame can be animating in one panel and frozen in the other.
///
/// Both spellings of the concurrency-slot keyword are listed deliberately: the
/// gate reports a slot wait as lowercase status traffic
/// (`"queued for a backend slot (n of m) …"`) while the Subagents panel labels
/// the same state `(Queued n/m)`. A round in which every session is waiting for
/// a slot has to keep animating exactly like a streaming round, and a Manager
/// turn that waits for a slot has no subagent row at all — so for that case this
/// keyword list, not the `is_active` scan, is the only thing that can hold the
/// spinner up.
pub(crate) const ACTIVITY_STATUS_KEYWORDS: &[&str] = &[
    "Running",
    "Delegating",
    "calling backend",
    "calling model",
    "Starting",
    "streaming",
    "thinking",
    "Arbitrating",
    "queued",
    "Queued",
    // Both spellings of the arbitration-pause keyword, for the same reason as the
    // queue pair: the preemption handshake reports a yielded slot as lowercase
    // status traffic (`"… Yielded model slot (…) to Steer Arbitrator — stream
    // paused"`) while the Subagents panel labels the same row `(Paused)`. A round
    // whose streams are parked behind the arbitrator is still an in-flight round —
    // the work resumes the moment the slot is handed back — so it must keep
    // animating exactly like a streaming round.
    "paused",
    "Paused",
];

/// One error kept in renderer state so the failure outlives the status line that
/// carried it (spec §8.5).
///
/// Two parts, both verbatim copies of reported traffic:
///
/// * [`Self::category`] — the categorical cause the session loop reported
///   (`helpers::classify_llm_error` produces the vocabulary: `http`, `connectivity`,
///   `timeout`, `stream`, `unknown`; `System Error` is the transport/panic class).
///   This is the half a one-line status can show.
/// * [`Self::detail`] — the **whole** reported text, multi-line included. §7.4 keeps
///   only the first line in `status_line`; the buffer keeps all of it, which is what
///   makes the recall worth printing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StickyError {
    /// The categorical cause, e.g. `http` for `LLM error: http (Ready)`.
    pub(crate) category: String,
    /// The full error text, verbatim.
    pub(crate) detail: String,
}

/// Whether one line is error traffic this renderer should remember.
///
/// The spellings are exactly the ones `events.rs::error_line` recognises when `e`
/// recalls (the `e`/`/error` handler is frozen), plus nothing else: recording a line
/// the recall path cannot reach would create a second truth the key path never
/// reads. Every line matched here is a line the session loop pushes into the
/// transcript, so the remembered error and the recalled error are the same bytes.
fn is_error_traffic_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with("LLM error")
        || trimmed.starts_with("System Error")
        || trimmed.starts_with('✗')
        || trimmed.contains("LLM error:")
}

/// Project one piece of error traffic onto the §8.5 buffer, or `None` when the text
/// carries no error traffic at all.
///
/// The category is read out of the categorical spelling the loop emits
/// (`LLM error: <category> …`); when a line carries no category clause the line
/// itself is the category rather than a guessed one.
fn sticky_error_from(text: &str) -> Option<StickyError> {
    let line = text.lines().find(|line| is_error_traffic_line(line))?;
    let line = line.trim();
    let category = match line.to_ascii_lowercase().split_once("llm error:") {
        Some((_, rest)) => rest
            .trim()
            .split(['(', ':'])
            .next()
            .map(str::trim)
            .filter(|word| !word.is_empty())
            .unwrap_or("llm error")
            .to_string(),
        None if line.starts_with("System Error") => "System Error".to_string(),
        None if line.starts_with('✗') => line
            .trim_start_matches('✗')
            .split_whitespace()
            .next()
            .unwrap_or("error")
            .to_string(),
        None => line.to_string(),
    };
    Some(StickyError {
        category,
        detail: text.to_string(),
    })
}

/// Mirror the worker lifecycle the registry already carries onto the pane rows
/// (plumbing delta D1, design §9.3).
///
/// Everything below already exists on [`crate::orchestrator::WorkerEntry`]
/// (`status_label`, `implementation_turns`, `validation_rounds`,
/// `latest_validator_feedback`) and used to be dropped at the UI boundary, which is
/// why a finished row could only ever read `✓ done` and why the `val N` field stayed
/// switched off (§8.4 gate). One registry snapshot per pass, joined on the
/// **effective registry key** — the same key the row projection uses — so no label
/// string is parsed and no counter is invented here.
///
/// A row whose key the registry no longer carries keeps its last mirror: the
/// completed ring is what supplies a terminal label (`Aborted`, `Rejected`), and an
/// entry leaving the ring must not erase the state that explains why the row closed.
fn fold_worker_lifecycle(rows: &mut [SubagentDetail]) {
    if rows.is_empty() {
        return;
    }
    let entries = crate::orchestrator::worker_entries();
    for row in rows.iter_mut() {
        let Some(entry) = entries.iter().find(|entry| entry.key == row.name) else {
            continue;
        };
        row.status_label = Some(entry.status_label.clone());
        row.implementation_turns = entry.implementation_turns;
        row.validation_rounds = entry.validation_rounds;
        row.latest_validator_feedback = entry.latest_validator_feedback.clone();
    }
}

pub struct TuiRenderer {
    /// Ordered chat/log transcript. Each entry is a single message string
    /// (may contain embedded `\n`).
    pub(crate) messages: Vec<String>,
    /// Streaming assistant "thinking" block (between ` thinking` and ` response`).
    pub(crate) current_thought: String,
    /// Streaming assistant content (final answer text).
    pub(crate) current_content: String,
    /// The execution plan text. Defaults to `"No active execution plan."`.
    pub(crate) plan_content: String,
    /// Multi-line text editor widget for input.
    pub(crate) textarea: ratatui_textarea::TextArea<'static>,
    /// The current text in the input box.
    pub(crate) input_text: String,
    /// Byte offset of the input cursor within `input_text` (always on a char
    /// boundary). Drives insert/delete and the rendered caret position.
    pub(crate) cursor: usize,
    /// Whether the user is in the "confirm abort" state.
    pub(crate) confirm_abort: bool,
    /// Timestamp when confirm-abort was armed. Abort expires after 3 seconds.
    pub(crate) abort_armed_at: Option<std::time::Instant>,
    /// Whether UI state has changed and needs a flush/redraw.
    pub(crate) dirty: bool,
    /// Abort / user-exit flags (the `Renderer` trait's abort surface).
    pub(crate) input_state: crate::ui::InputState,
    /// Frame counter for animations (e.g. status spinner).
    pub(crate) frame_counter: u64,
    /// The status bar text. Defaults to `"Ready"`.
    pub(crate) status_line: String,
    /// Which legacy panel has focus (drives the border colour of the pre-rebuild
    /// panes and is the persisted mirror of [`Self::focus`]). Defaults to `Chat`.
    /// Retired with the key path (spec §9.2); t-011 replaces it with
    /// [`Self::focus`], and `adopt_legacy_view_writes` keeps the two honest while
    /// `events.rs` still writes this field directly.
    pub(crate) focused_panel: FocusedPanel,
    /// M1–M4 view mode — **real UI state** (spec §4.1), not a reading of the pane
    /// toggles. Built once in [`Self::new`] and moved only by
    /// [`Self::set_view_mode`] (`F2`–`F5` and `/mode` call it in t-011).
    pub(crate) view_mode: state::ViewMode,
    /// Which pane holds focus (spec §4.2) — **real UI state**. Built once in
    /// [`Self::new`] (the composer: printable keys must always reach it, §4.4)
    /// and moved only by [`Self::set_focus`] / [`Self::cycle_focus_pane`].
    pub(crate) focus: state::FocusPane,
    /// Transcript follow: `LIVE` vs `↓ N new` (spec G7) — **real UI state**. The
    /// legacy `chat_auto_scroll` flag is its mirror for the pre-rebuild scroll
    /// path; [`Self::set_follow`] writes both.
    pub(crate) follow: bool,
    /// The modal surface on screen (spec §3, §8.5) — **real UI state**: `None`,
    /// the `?`/`F1` legend, or a confirm awaiting `y`/`n`. Moved only by
    /// [`Self::open_help`], [`Self::show_confirm`] and [`Self::close_overlay`].
    pub(crate) overlay: state::OverlayState,
    /// The `?`/`F1` legend toggle (G2). It stays on while a confirm surface is
    /// drawn over it, so dismissing a confirm returns the legend (§5: `Esc`
    /// closes one overlay). Replaces the `help_open = false` the view model used
    /// to hard-code.
    pub(crate) help_open: bool,
    /// A destructive action is awaiting `y`/`n` (spec G3, §3 confirm overlay).
    /// The armed-abort prompt of §8.5 occupies the same surface while its 3 s
    /// window is live; `close_overlay` dismisses whichever surface is on top.
    pub(crate) confirm_open: bool,
    /// Which request the confirm surface is holding (spec §3, G3): the tag the key
    /// path needs so `y` runs exactly what `x` or `Ctrl+D` asked for instead of
    /// re-guessing it from whatever pane happens to be focused. `None` means the
    /// surface on screen is the armed-abort prompt, which honours no `y`/`n` at all
    /// (§5 disarms on printables).
    pub(crate) confirm_action: Option<events::ConfirmAction>,
    /// What [`Self::adopt_legacy_view_writes`] last saw on the legacy view
    /// fields, so a write the transition helpers made is never mistaken for a
    /// write the legacy key path made.
    pub(crate) legacy_view: LegacyViewMirror,
    /// Whether teardown has run. Set by [`Self::shutdown`]; while it is set the
    /// renderer never paints (see [`Self::render_terminal`]), so a post-teardown
    /// `flush` cannot draw TUI chrome into the user's shell and a second
    /// `shutdown` — or the `Drop` that follows one — is a no-op paint-wise.
    pub(crate) terminal_torn_down: bool,
    /// Vertical scroll offset of the chat panel.
    pub(crate) chat_scroll: u16,
    /// Vertical scroll offset of the plan panel.
    pub(crate) plan_scroll: u16,
    /// Whether the plan panel is visible. Defaults to `false`.
    pub(crate) show_plan_panel: bool,
    /// Track if an active plan was previously open on disk for auto-open/close.
    pub(crate) had_active_plan: bool,
    /// Whether ` thinking`/` response` blocks are displayed. Defaults to `false`.
    pub(crate) show_thought: bool,
    /// Whether the subagents panel is visible. Defaults to `false`.
    pub(crate) show_subagent_panel: bool,
    pub(crate) subagents: Vec<SubagentDetail>,
    /// Defaults to `0`.
    pub(crate) selected_subagent_idx: usize,
    /// Scroll offset of the subagent details pane.
    pub(crate) subagent_scroll: u16,
    /// Scroll offset of the subagent list.
    pub(crate) subagent_list_scroll: u16,
    /// Input command history (most recent last).
    pub(crate) history: Vec<String>,
    /// Current position in history navigation. `None` = not navigating.
    pub(crate) history_index: Option<usize>,
    /// Saved draft of `input_text` when history navigation begins.
    pub(crate) input_draft: String,
    /// Buffer for streaming steer-mode sentences awaiting completion.
    pub(crate) steer_sentence_buffer: String,
    /// What happened to the operator's own steering instructions (spec §3, §8.3;
    /// DATA §6 gap 7).
    ///
    /// Every entry was **handed to the renderer by the bridge** at the statement where the
    /// bridge touches its own steer queue, files a notice, or refuses a target — see
    /// [`crate::ui::Renderer::note_steer`]. The renderer never invents an entry, never
    /// re-decides one, and never counts anything that is not in this list: the status-bar
    /// `N steers` chip is `queued_steers()`, the length of the `Queued` slice of this very
    /// vector. Bounded by [`MAX_STEER_TRACES`] so a long session cannot grow it without end.
    pub(crate) steer_traces: Vec<SteerTrace>,

    /// Cached chat inner width (area width − 2). Defaults to `80`.
    pub(crate) chat_width: std::cell::Cell<usize>,
    /// Cached chat inner height (area height − 2). Defaults to `15`.
    pub(crate) chat_height: std::cell::Cell<usize>,
    /// Cached subagent details width. Defaults to `40`.
    pub(crate) subagent_width: std::cell::Cell<usize>,
    /// Cached subagent details height. Defaults to `15`.
    pub(crate) subagent_height: std::cell::Cell<usize>,
    /// `config.max_context_tokens`: the denominator of the `ctx %` chip (spec
    /// §2.2). `0` means no config was ever handed over, and the panes then print
    /// the raw token count instead of a percentage of an unknown budget.
    pub(crate) max_context_tokens: usize,
    /// Wrapped transcript lines the body reported on the frame where the viewport
    /// last sat at its bottom — the baseline for the `↓ N new` count (spec G7).
    pub(crate) transcript_baseline_lines: Option<usize>,
    /// The §8.5 sticky last-error buffer: the categorical cause and the **full**
    /// error text of the most recent error this renderer saw.
    ///
    /// `status_line` keeps only the first line of the traffic that produced it
    /// (spec §7.4), so without this buffer the cause a failure was reported with is
    /// overwritten by the next status line and `e` / `/error` have nothing left to
    /// recall. The buffer stores the text **verbatim** — it never rephrases an error
    /// and never invents a cause that was not reported.
    pub(crate) last_error: Option<StickyError>,
    /// Width used for the current line cache. Defaults to `0`.
    pub(crate) cached_chat_width: std::cell::Cell<usize>,
    /// `show_thought` value used for the current line cache.
    pub(crate) cached_show_thought: std::cell::Cell<bool>,
    /// Per-message cached wrapped-line counts.
    pub(crate) cached_message_lines: std::cell::RefCell<Vec<usize>>,
    /// Sum of all cached per-message line counts.
    pub(crate) cached_total_message_lines: std::cell::Cell<usize>,
    /// Styled lines cached per message in `messages` so markdown/ANSI is not re-parsed on every frame.
    pub(crate) cached_rendered_messages: std::cell::RefCell<Vec<Vec<ratatui::text::Line<'static>>>>,
    /// `show_thought` value that the rendered-message cache was built with.
    pub(crate) cached_rendered_show_thought: std::cell::Cell<bool>,
    /// Whether subagent details auto-scroll. Defaults to `false`.
    pub(crate) subagent_autoscroll: bool,

    /// Completed input lines forwarded to the session loop.
    pub(crate) rx: Receiver<String>,
    pub(crate) tx: Sender<String>,
    /// Persistent terminal instance to preserve Ratatui frame diff buffers.
    pub(crate) terminal: Option<Terminal<CrosstermBackend<io::Stdout>>>,

    pub(crate) chat_auto_scroll: bool,
    pub(crate) plan_max_scroll: u16,
    pub(crate) plan_auto_scroll: bool,
    pub(crate) active_plan_task: Option<String>,

    pub(crate) chat_area: Rect,
    pub(crate) plan_area: Rect,
    pub(crate) subagent_area: Rect,
    pub(crate) input_area: Rect,

    pub(crate) active_agent: String,
    pub(crate) last_render: std::time::Instant,
    pub(crate) waiting_for_token_since: Option<std::time::Instant>,
    pub(crate) last_plan_check: std::time::Instant,
    pub(crate) plan_mtime: Option<std::time::SystemTime>,
    pub(crate) archive_mtime: Option<std::time::SystemTime>,
    pub(crate) plan_is_archived: bool,
    pub(crate) session_start: std::time::Instant,

    /// Estimated total input (prompt) tokens across the session.
    pub tokens_in: usize,
    /// Estimated total output (completion) tokens across the session.
    pub tokens_out: usize,
    /// Context window size (tokens) of the orchestrator/main turn.
    pub orchestrator_context_tokens: usize,
    /// Map of specialist role -> max_thinking_tokens.
    pub(crate) thinking_budgets: std::collections::HashMap<String, usize>,
    /// Default thinking budget for specialists without specific overrides.
    pub(crate) default_thinking_budget: usize,
    /// Thinking content accumulated during the current active turn per subagent.
    pub(crate) subagent_turn_thinking: std::collections::HashMap<String, String>,
    /// Whether the subagent is actively streaming reasoning tokens in the current turn.
    pub(crate) subagent_is_thinking: std::collections::HashMap<String, bool>,
    /// Optional local override or cache of execution plan start instant.
    pub(crate) plan_start_time: Option<std::time::Instant>,
    /// Optional local override or cache of execution plan completed instant.
    pub(crate) plan_completed_time: Option<std::time::Instant>,
}

impl TuiRenderer {
    pub fn new() -> Self {
        let (tx, rx) = channel::<String>();
        Self {
            messages: Vec::new(),
            current_thought: String::new(),
            current_content: String::new(),
            plan_content: "No active execution plan.".to_string(),
            textarea: ratatui_textarea::TextArea::default(),
            input_text: String::new(),
            cursor: 0,
            confirm_abort: false,
            abort_armed_at: None,
            dirty: true,
            input_state: crate::ui::InputState::default(),
            frame_counter: 0,
            status_line: "Ready".to_string(),
            tokens_in: 0,
            tokens_out: 0,
            orchestrator_context_tokens: 0,
            focused_panel: FocusedPanel::Chat,
            // The view state is built once, here, and only the transition
            // helpers below move it (spec §4.1, §4.2). The mode is the mode the
            // boot pane inventory describes (§4.3): both side panes boot closed,
            // which is M1; `F2`/`F3`/`F4`/`F5`, `/mode`, `Ctrl+P` and `Ctrl+A`
            // move it, and `set_view_mode(Split)` opens both side panes with it.
            view_mode: state::ViewMode::for_panes(false, false),
            // The composer holds focus at boot: printable keys must reach it in
            // every mode and every focus state (§4.4), so it is where the operator
            // starts and what `Tab` cycles back to (§4.2).
            focus: state::FocusPane::Input,
            // Follow is on at boot: the transcript shows the newest line until the
            // operator scrolls back, and only then starts counting (§G7).
            follow: true,
            overlay: state::OverlayState::None,
            help_open: false,
            confirm_open: false,
            confirm_action: None,
            legacy_view: LegacyViewMirror {
                plan_visible: false,
                workers_visible: false,
                focus: FocusedPanel::Chat,
                follow: true,
                abort_armed: false,
            },
            terminal_torn_down: false,
            chat_scroll: 0,
            plan_scroll: 0,
            show_plan_panel: false,
            had_active_plan: false,
            show_thought: false,
            show_subagent_panel: false,
            subagents: Vec::new(),
            selected_subagent_idx: 0,
            subagent_scroll: 0,
            subagent_list_scroll: 0,
            history: Vec::new(),
            history_index: None,
            input_draft: String::new(),
            steer_sentence_buffer: String::new(),
            steer_traces: Vec::new(),

            chat_width: std::cell::Cell::new(80),
            chat_height: std::cell::Cell::new(15),
            subagent_width: std::cell::Cell::new(40),
            subagent_height: std::cell::Cell::new(15),
            max_context_tokens: 0,
            transcript_baseline_lines: None,
            last_error: None,
            cached_chat_width: std::cell::Cell::new(0),
            cached_show_thought: std::cell::Cell::new(false),
            cached_message_lines: std::cell::RefCell::new(Vec::new()),
            cached_total_message_lines: std::cell::Cell::new(0),
            cached_rendered_messages: std::cell::RefCell::new(Vec::new()),
            cached_rendered_show_thought: std::cell::Cell::new(false),
            subagent_autoscroll: true,

            rx,
            tx,
            terminal: None,

            chat_auto_scroll: true,
            plan_max_scroll: 0,
            plan_auto_scroll: false,
            active_plan_task: None,

            chat_area: Rect::default(),
            plan_area: Rect::default(),
            subagent_area: Rect::default(),
            input_area: Rect::default(),

            active_agent: "Manager".to_string(),
            last_render: std::time::Instant::now(),
            waiting_for_token_since: None,
            last_plan_check: std::time::Instant::now()
                .checked_sub(Duration::from_secs(10))
                .unwrap_or_else(std::time::Instant::now),
            plan_mtime: None,
            archive_mtime: None,
            plan_is_archived: false,
            session_start: std::time::Instant::now(),
            thinking_budgets: std::collections::HashMap::new(),
            default_thinking_budget: crate::config::DEFAULT_MAX_THINKING_TOKENS,
            subagent_turn_thinking: std::collections::HashMap::new(),
            subagent_is_thinking: std::collections::HashMap::new(),
            plan_start_time: None,
            plan_completed_time: None,
        }
    }

    /// Populate thinking budget limits per specialist and globally from config.
    ///
    /// The context budget (`max_context_tokens`) rides the same hand-over: it is
    /// the denominator of the `ctx %` chip both the header and the status bar print
    /// (spec §2.2), and this is the only place a `Config` reaches the renderer.
    pub fn set_thinking_budgets(&mut self, cfg: &Config) {
        let global_budget = if cfg.max_thinking_tokens != crate::config::DEFAULT_MAX_THINKING_TOKENS
        {
            cfg.max_thinking_tokens
        } else {
            cfg.monitoring
                .as_ref()
                .map(|m| m.max_thinking_tokens)
                .unwrap_or(cfg.max_thinking_tokens)
        };
        self.default_thinking_budget = global_budget;
        self.max_context_tokens = cfg.max_context_tokens;
        self.thinking_budgets.clear();
        for (role, spec) in &cfg.orchestration.specialists {
            if let Some(b) = spec.max_thinking_tokens {
                self.thinking_budgets.insert(role.clone(), b);
            }
        }
    }

    /// Resolve the thinking token budget for a given subagent.
    pub(crate) fn get_thinking_budget(&self, sa: &SubagentDetail) -> usize {
        if let Some(b) = self.thinking_budgets.get(&sa.name) {
            return *b;
        }
        let role = sa.name.split('-').next().unwrap_or(&sa.name);
        if let Some(b) = self.thinking_budgets.get(role) {
            return *b;
        }
        self.default_thinking_budget
    }

    /// Total duration spent working on an execution plan.
    /// Only counts when actively working on a plan; freezes as soon as the plan is completed / archived.
    pub(crate) fn total_project_elapsed(&self) -> Option<std::time::Duration> {
        let using_global_plan = self.plan_start_time.is_none();
        let (inst, _) = self
            .plan_start_time
            .map(|t| (t, chrono::Local::now()))
            .or_else(|| {
                if self.had_active_plan {
                    crate::manager::phase::get_plan_start_time()
                } else {
                    None
                }
            })?;
        let global_completed = if using_global_plan && self.had_active_plan {
            crate::manager::phase::get_plan_completed_time()
        } else {
            None
        };
        if let Some(comp) = self.plan_completed_time.or(global_completed) {
            Some(comp.checked_duration_since(inst).unwrap_or_default())
        } else if self.plan_is_archived {
            if using_global_plan && self.had_active_plan {
                crate::manager::phase::record_plan_completed();
            }
            Some(inst.elapsed())
        } else {
            if using_global_plan && self.had_active_plan {
                let plan = crate::manager::phase::Plan::default();
                if plan.is_complete() {
                    crate::manager::phase::record_plan_completed();
                    if let Some(comp) = crate::manager::phase::get_plan_completed_time() {
                        return Some(comp.checked_duration_since(inst).unwrap_or_default());
                    }
                }
            }
            Some(inst.elapsed())
        }
    }

    /// The plan-start instant the plan box is actually drawing: the local
    /// override if the renderer holds one, otherwise the process-global plan
    /// start that [`Self::total_project_elapsed`] falls back to. Capture uses
    /// this so the stored timer is the one the pane showed, not whichever field
    /// happened to be populated.
    fn plan_start_instant(&self) -> Option<std::time::Instant> {
        self.plan_start_time.or_else(|| {
            if self.had_active_plan {
                crate::manager::phase::get_plan_start_time().map(|(instant, _wall)| instant)
            } else {
                None
            }
        })
    }

    /// The plan-completed instant the box is drawing, with the same fallback
    /// order as [`Self::total_project_elapsed`]: the local override, then the
    /// global completion only when the box is reading the global start.
    fn plan_completed_instant(&self) -> Option<std::time::Instant> {
        self.plan_completed_time.or_else(|| {
            if self.plan_start_time.is_none() && self.had_active_plan {
                crate::manager::phase::get_plan_completed_time()
            } else {
                None
            }
        })
    }

    /// Paint one frame through the routed draw path (spec §9.1: the new UI is the
    /// only thing this entry path paints).
    ///
    /// After teardown the terminal is gone — raw mode is off, the primary screen is
    /// back and the operator's shell owns stdout — so this returns without painting
    /// instead of recreating a `Terminal` and writing escape codes into the shell.
    /// The session loop still calls `flush()` a few times after `shutdown()` (the
    /// shutdown is issued from inside the loop and from `Drop`), and every one of
    /// those calls must be a no-op rather than a repaint on a screen that is no
    /// longer ours.
    pub(crate) fn render_terminal(&mut self) -> Result<()> {
        if self.terminal_torn_down {
            return Ok(());
        }
        self.last_render = std::time::Instant::now();
        self.frame_counter = self.frame_counter.wrapping_add(1);
        let mut terminal = match self.terminal.take() {
            Some(t) => t,
            None => {
                let stdout = io::stdout();
                let backend = CrosstermBackend::new(stdout);
                Terminal::new(backend)?
            }
        };
        let res = self.draw(&mut terminal);
        self.terminal = Some(terminal);
        res
    }

    /// One frame of the rebuilt UI: derive the [`state::UiView`] view model,
    /// compute the pane geometry with [`layout::compute_layout`] (spec §2), then
    /// render every pane component of [`components`] (spec §3) and return the
    /// rects so [`Self::draw`] can write the geometry caches back.
    ///
    /// Each pane is drawn **exactly once, by its own component**: the component
    /// `render` functions own body and chrome together (body first, then the block
    /// that frames it), so there is no legacy content pass filling the new rects
    /// underneath a chrome pass any more. Every value a pane needs arrives in its
    /// `*Data` struct — the renderer hands the conversation, the plan text, the
    /// worker rows, the composer buffer, the session counters and its own clocks
    /// over, and reads the viewport each pass established back out of the report
    /// each `render` returns. The help overlay is drawn last, over the body (§7.3).
    pub(crate) fn draw_new_ui(&mut self, frame: &mut ratatui::Frame) -> layout::PaneRects {
        // The frame is built from stored view state, so anything a writer outside
        // the key path changed on the legacy fields (a worker activating opens the
        // Workers pane through `set_subagents`, a plan event opens the Plan pane,
        // the scroll keys move `chat_auto_scroll`) is folded into that state here,
        // immediately before the view model is read. One pass per frame, and the
        // mirror makes every fold an edge rather than a re-read.
        self.adopt_legacy_view_writes();
        // The notice tables are the only owner of what a filed notice did next, so the
        // ledger is re-read against them once per frame before anything reads it.
        self.refresh_steer_traces();
        let view = state::UiView::from_renderer(self);
        let area = frame.area();
        let rects = layout::compute_layout(area, &view);

        if let Some(notice) = rects.notice {
            components::render_notice(frame, notice, area.width, area.height);
            return rects;
        }

        let compact = layout::compact_legend(area.width);
        let elapsed = self.elapsed_clock();
        // `val N` (§2.2): the maximum `validation_rounds` over the projected worker
        // rows (plumbing delta D1, §9.3). One read per frame, shared by the header and
        // the status row so the two can never print two different rounds.
        let val_rounds = self.max_validation_rounds();

        // 1. Header: one borderless row of §2.2 chips, §7.4 drop ladder.
        if let Some(rect) = rects.header {
            let data = components::header::HeaderData {
                mode: rects.mode,
                plan: &view.plan,
                budget: layout::header_budget(area.width),
                ctx_tokens: self.orchestrator_context_tokens,
                ctx_limit: self.max_context_tokens,
                elapsed,
                val_rounds,
            };
            components::header::render(frame, rect, &data);
        }

        // 2. Transcript. The pane is handed the conversation and the renderer's own
        // rendered-line cache, and reports back where the viewport landed; the
        // write-back keeps `chat_scroll` / `chat_width` / `chat_height` — which the
        // autoscroll preparation above and the scroll keys in `events.rs` read —
        // pointing at what was actually drawn. The unseen-line count stays inside
        // the pane: it is the transcript's own scroll state, and the status row that
        // used to echo it put a second `● LIVE` on the frame.
        if let Some(rect) = rects.transcript {
            self.ensure_rendered_message_cache();
            let report = {
                let body = components::transcript::TranscriptBody {
                    messages: &self.messages,
                    rendered: Some(&self.cached_rendered_messages.borrow()),
                    thought: &self.current_thought,
                    content: &self.current_content,
                    steer: &self.steer_sentence_buffer,
                    show_thought: self.show_thought,
                };
                let data = components::transcript::TranscriptData {
                    focus: view.focused(state::FocusPane::Transcript),
                    follow: view.follow,
                    scroll: view.scroll.transcript,
                    body,
                    unseen_baseline: self.transcript_baseline_lines,
                };
                components::transcript::render(frame, rect, &data)
            };
            self.chat_width
                .set(usize::from(rect.width.saturating_sub(2)));
            self.chat_height
                .set(usize::from(rect.height.saturating_sub(2)));
            self.chat_scroll = report.scroll;
            // The `↓ N new` baseline (G7): whenever the viewport sits at the bottom
            // of the body, every line that exists has been on screen, so the body's
            // own line count becomes the baseline the next frame measures growth
            // against.
            if view.follow || usize::from(report.scroll) >= report.max_scroll {
                self.transcript_baseline_lines = Some(report.total_lines);
            }
        }

        // 3. Execution Plan: body, chrome, and the scroll geometry that
        // `plan_auto_scroll` and the `↑`/`↓` clamps share.
        if let Some(rect) = rects.plan {
            let active_tasks = self.active_plan_tasks();
            let target_task = self.plan_target_task();
            let geometry = {
                let data = components::plan_pane::PlanData {
                    focus: view.focused(state::FocusPane::Plan),
                    progress: &view.plan,
                    scroll: view.scroll.plan,
                    text: &self.plan_content,
                    active_tasks: &active_tasks,
                    auto_scroll: self.plan_auto_scroll,
                    target_task: target_task.as_deref(),
                };
                components::plan_pane::render(frame, rect, &data)
            };
            self.plan_scroll = geometry.scroll;
            self.plan_max_scroll = geometry.max_scroll;
        }

        // 4. Workers: roster + detail half, with both offsets and the detail
        // measure written back for the autoscroll estimate and the key clamps.
        if let Some(rect) = rects.workers {
            let selected = self
                .selected_subagent_idx
                .min(self.subagents.len().saturating_sub(1));
            let thinking_budget = self
                .subagents
                .get(selected)
                .map(|row| self.get_thinking_budget(row));
            // The steering notices waiting in front of the selected worker, read from
            // the orchestrator's own inbox (read-only: peeking never drains it). This is
            // the targeting half of the affordance — which instruction the arbitrator
            // routed to *this* specialist, and how long it has been waiting.
            let steering = self
                .subagents
                .get(selected)
                .map(|row| crate::orchestrator::notice::peek_worker_inbox(&row.name))
                .unwrap_or_default();
            let geometry = {
                let data = components::activity::ActivityData {
                    focus: view.focused(state::FocusPane::Workers),
                    rows: &self.subagents,
                    selected: self.selected_subagent_idx,
                    detail_present: layout::workers_detail_present(area.width),
                    scroll: view.scroll.workers,
                    list_scroll: self.subagent_list_scroll,
                    show_thought: self.show_thought,
                    thinking_budget,
                    steering: &steering,
                };
                components::activity::render(frame, rect, &data)
            };
            self.subagent_list_scroll = geometry.roster_scroll;
            self.subagent_scroll = geometry.detail_scroll;
            if geometry.detail_present {
                self.subagent_width.set(geometry.detail_width);
                self.subagent_height.set(geometry.detail_height);
            }
        }

        // 5. Composer: the `TextArea` body first, then the frame that owns every
        // border row. `prepare_textarea` synchronises the widget and strips the
        // block off it, which is what lets a single border be drawn.
        if let Some(rect) = rects.input {
            self.prepare_textarea();
            let buffer = self.textarea.lines().join("\n");
            let history = self.history_position();
            let data = components::input::InputData {
                focus: view.focused(state::FocusPane::Input),
                active: view.composer_active(),
                armed: view.abort_armed,
                draft_lines: view.input_lines,
                buffer: Some(&buffer),
                history,
                textarea: Some(&self.textarea),
            };
            components::input::render(frame, rect, &data);
        }

        // 6. Status: metrics row + chip row, from the session counters the renderer
        // owns and the pane rects this frame actually produced — a pane the §2.5
        // ladder took away must not still be announced as `on` (§2.5, G5).
        if let Some(rect) = rects.status {
            let (global_in, global_out) = crate::llm::get_global_token_counts();
            let data = components::status_bar::StatusData {
                workers: &self.subagents,
                show_thought: view.show_thought,
                plan_visible: view.plan_visible,
                workers_visible: view.workers_visible,
                plan_present: rects.plan.is_some(),
                workers_present: rects.workers.is_some(),
                abort_armed: view.abort_armed,
                tokens_in: self.tokens_in.max(global_in),
                tokens_out: self.tokens_out.max(global_out),
                ctx_tokens: self.orchestrator_context_tokens,
                ctx_limit: self.max_context_tokens,
                elapsed,
                val_rounds,
                queued_steers: self.queued_steers(),
            };
            components::status_bar::render(frame, rect, &data);
        }

        // 7. Footer legend: `components::help::footer_legend` is the single source
        // for this row, the same table the help overlay reads (§6, §7.3).
        if let Some(rect) = rects.footer {
            let data = components::help::HelpData {
                kind: components::help::HelpKind::Legend,
                compact,
                confirm: None,
            };
            components::help::render(frame, rect, &data);
        }
        // 8. Overlays draw last, centred over the body (§7.3): `Clear` plus a
        // content-sized bordered `Paragraph` clamped to 80 % of this rect. Only the
        // body is covered — the header, the two status rows, the composer and the
        // footer legend are already painted and stay readable underneath, which is
        // what keeps the live metrics counting while a confirm is on screen (§7.3,
        // G4) and leaves the armed prompt on status row 2. A pending request names
        // itself on the surface (its action and its consequence, plus the `y` / `n`
        // affordance); with no request the surface is the armed-abort prompt of
        // §8.5, which honours no `y`/`n` (§5).
        if view.overlay != state::OverlayState::None
            && let Some(rect) = rects.body
        {
            let request = self
                .confirm_action
                .as_ref()
                .filter(|_| view.overlay == state::OverlayState::Confirm)
                .map(|action| action.describe());
            let data = components::help::HelpData {
                kind: components::help::HelpKind::Overlay(view.overlay),
                compact,
                confirm: request,
            };
            components::help::render(frame, rect, &data);
        }
        rects
    }

    /// Task ids the plan pane must highlight: every active worker's task plus the
    /// task the plan itself names in flight, cleaned and deduplicated. The set the
    /// legacy plan pane built inline (`render.rs:458-485`) — moved here so the pane
    /// receives it as data.
    pub(crate) fn active_plan_tasks(&self) -> Vec<String> {
        let mut active: Vec<String> = Vec::new();
        for sa in &self.subagents {
            if !sa.is_active {
                continue;
            }
            let tid = sa
                .task_id
                .as_deref()
                .or_else(|| sa.name.find("-t-").map(|pos| &sa.name[pos + 1..]));
            if let Some(tid) = tid {
                let clean = crate::ui::tui::formatting::clean_task_id(tid);
                if !clean.is_empty() && !active.iter().any(|t| t.eq_ignore_ascii_case(clean)) {
                    active.push(clean.to_string());
                }
            }
        }
        if let Some(tid) = &self.active_plan_task {
            let clean = crate::ui::tui::formatting::clean_task_id(tid);
            if !clean.is_empty() && !active.iter().any(|t| t.eq_ignore_ascii_case(clean)) {
                active.push(clean.to_string());
            }
        }
        active
    }

    /// The task the plan auto-scroll centres on: the plan's own active task, else
    /// the task of the first active worker.
    pub(crate) fn plan_target_task(&self) -> Option<String> {
        self.active_plan_task.clone().or_else(|| {
            self.subagents
                .iter()
                .find(|sa| sa.is_active && sa.task_id.is_some())
                .and_then(|sa| sa.task_id.clone())
        })
    }

    /// Where the composer is standing in the input history, for the `history n/m`
    /// chip (spec §5). `None` while the operator is editing their own draft.
    pub(crate) fn history_position(&self) -> Option<components::input::HistoryPos> {
        let index = self.history_index?;
        let len = self.history.len();
        if len == 0 || index >= len {
            return None;
        }
        Some(components::input::HistoryPos {
            entry: index + 1,
            len,
        })
    }

    /// Get the composer's `TextArea` ready for a component-owned body pass: the
    /// widget is synchronised with `input_text`, its cursor line carries no style
    /// of its own, and its block is cleared — the composer component draws the one
    /// border, title and chip row, so a block on the widget would paint a second
    /// frame over it.
    pub(crate) fn prepare_textarea(&mut self) {
        self.ensure_textarea_in_sync();
        self.textarea.set_block(ratatui::widgets::Block::default());
        self.textarea
            .set_cursor_line_style(ratatui::style::Style::default());
    }

    /// The elapsed clock both chrome panes print, as whole seconds.
    ///
    /// The execution-plan project clock while one owns the run — the value the
    /// legacy status bar right-aligned, freezing at plan completion — and the
    /// session clock (`session_start`, spec §3) otherwise, so a session that has
    /// not reached a plan yet still counts up instead of printing nothing.
    pub(crate) fn elapsed_clock(&self) -> Option<u64> {
        self.total_project_elapsed()
            .map(|elapsed| elapsed.as_secs())
            .or_else(|| Some(self.session_start.elapsed().as_secs()))
    }

    /// The §2.2 `val N` value: the highest `validation_rounds` across the projected
    /// worker rows (plumbing delta D1, §9.3). `0` means the registry recorded no
    /// validation round for any worker, and the panes withhold the field rather than
    /// print `val 0`. Never derived from an arbitration round (§8.4).
    pub(crate) fn max_validation_rounds(&self) -> usize {
        self.subagents
            .iter()
            .map(|row| row.validation_rounds)
            .max()
            .unwrap_or(0)
    }

    // ---- steering-instruction lifecycle (spec §3, §8.3; DATA §6 gap 7) ------
    //
    // The queue a steering instruction lands in is a local of the session loop and the
    // notices it becomes live in the orchestrator's bounded inboxes, so the only thing
    // the renderer may show is what the bridge *tells* it at the statement that touches
    // either. `record_steer` is fed exclusively by
    // [`crate::ui::Renderer::note_steer`]; `refresh_steer_traces` then asks the
    // orchestrator's own read-only notice tables what became of a notice the bridge
    // filed, and adopts only an answer those tables actually give.

    /// Fold one bridge-reported transition into the ledger, announcing it in the
    /// transcript exactly once per stage change.
    ///
    /// The identity of an entry is the pair *instruction + addressee*, because one
    /// instruction can legitimately be filed for several specialists at once and each
    /// of those is a separate fact for the operator. The one exception is a stage that
    /// ends the wait: a delivered or refused instruction replaces the entry the bridge
    /// earlier reported as held for it, even when the addressee is named differently
    /// there — the wait for *that instruction* is over, and the chip must say so.
    pub(crate) fn record_steer(&mut self, trace: &SteerTrace) {
        let known = self
            .steer_traces
            .iter()
            .position(|entry| entry.text == trace.text && entry.target == trace.target)
            .or_else(|| {
                if trace.stage == SteerStage::Queued {
                    return None;
                }
                self.steer_traces
                    .iter()
                    .position(|entry| entry.stage == SteerStage::Queued && entry.text == trace.text)
            });
        match known {
            Some(index) if self.steer_traces[index].stage == trace.stage => {
                // Same stage: no new fact for the operator, only a sharper reason. The
                // transcript gets nothing, so an instruction never scrolls the pane with
                // restatements of the same state.
                self.steer_traces[index].reason = trace.reason.clone();
                if trace.notice_id.is_some() {
                    self.steer_traces[index].notice_id = trace.notice_id.clone();
                }
                self.dirty = true;
                return;
            }
            Some(index) => {
                self.steer_traces[index] = trace.clone();
            }
            None => {
                self.steer_traces.push(trace.clone());
                if self.steer_traces.len() > MAX_STEER_TRACES {
                    let excess = self.steer_traces.len() - MAX_STEER_TRACES;
                    self.steer_traces.drain(0..excess);
                }
            }
        }
        self.messages.push(steer_transcript_line(trace));
        if self.chat_auto_scroll {
            let w = self.chat_width.get();
            let n = self.estimated_chat_lines(w);
            let h = self.chat_height.get();
            self.chat_scroll = n.saturating_sub(h) as u16;
        }
        self.dirty = true;
    }

    /// What the orchestrator's notice tables now say about the notices the bridge filed,
    /// folded back into the same ledger (never into a second one).
    ///
    /// Three answers exist and only these: the notice is still pending and past its TTL
    /// (it will never be read → [`SteerStage::Revoked`]), a reply for it was recorded
    /// ([`SteerStage::Delivered`]), or the tables say nothing new (the entry keeps the
    /// stage the bridge gave it). A missing notice is never read as a revocation: the
    /// inboxes are drained by the worker itself, and a drained notice was delivered.
    pub(crate) fn refresh_steer_traces(&mut self) {
        let updates: Vec<SteerTrace> = self
            .steer_traces
            .iter()
            .filter_map(|entry| {
                let notice_id = entry.notice_id.as_deref()?;
                let pending = crate::orchestrator::get_pending_notice(notice_id);
                let reply = crate::orchestrator::get_worker_reply(notice_id);
                if reply.is_some() && entry.stage != SteerStage::Delivered {
                    return Some(SteerTrace {
                        stage: SteerStage::Delivered,
                        text: entry.text.clone(),
                        target: entry.target.clone(),
                        notice_id: entry.notice_id.clone(),
                        reason: "the specialist answered it".to_string(),
                    });
                }
                if let Some(notice) = pending
                    && crate::orchestrator::notice::is_notice_expired(&notice)
                    && entry.stage != SteerStage::Revoked
                {
                    return Some(SteerTrace {
                        stage: SteerStage::Revoked,
                        text: entry.text.clone(),
                        target: entry.target.clone(),
                        notice_id: entry.notice_id.clone(),
                        reason: format!(
                            "it passed its lifetime in {}'s inbox unanswered",
                            entry.target
                        ),
                    });
                }
                None
            })
            .collect();
        for update in updates {
            self.record_steer(&update);
        }
    }

    /// The `N steers` chip: how many of the operator's instructions are **still held**
    /// (the `Queued` slice of [`Self::steer_traces`]). There is no other source for this
    /// number and no other number is ever printed for it.
    pub(crate) fn queued_steers(&self) -> usize {
        self.steer_traces
            .iter()
            .filter(|entry| entry.stage == SteerStage::Queued)
            .count()
    }

    /// Check whether abort is actively armed and within the 3-second confirmation window.
    pub(crate) fn is_abort_armed(&self) -> bool {
        self.confirm_abort
            && self
                .abort_armed_at
                .is_some_and(|t| t.elapsed() < Duration::from_secs(3))
    }

    // ---- view-state transitions (spec §4.1, §4.2, §3, §8.5) -----------------
    //
    // `F2`–`F5`, `Tab`, `Ctrl+F`, `?`/`F1` and `Esc` call these in t-011; they
    // are the only way the view state moves, and every one of them leaves
    // `dirty` set so the next cadence tick repaints (spec §5: "no silent key
    // press", G5).

    /// Move to `mode` (spec §4.1: `F2` cycles, `F3`/`F4`/`F5` name one, and
    /// `/mode <name>` sets it directly).
    ///
    /// The two pane toggles follow the mode's pane inventory (§4.3) — they are
    /// the spelling the status-row chips, the legacy scroll path and the durable
    /// `ViewState` use, and keeping them in step is what stops one mode change
    /// from drawing a frame in which three sources disagree. A focus the new
    /// mode would orphan is repaired by cycling to the next pane that is on
    /// screen: an absent pane is never drawn and never hit-tested (§2.4.1), so a
    /// focus badge may never sit on one (G6).
    pub(crate) fn set_view_mode(&mut self, mode: state::ViewMode) {
        self.view_mode = mode;
        self.show_plan_panel = mode.plan_pane();
        self.show_subagent_panel = mode.workers_pane();
        self.legacy_view.plan_visible = self.show_plan_panel;
        self.legacy_view.workers_visible = self.show_subagent_panel;
        if !mode.presents(self.focus) {
            self.cycle_focus_pane(true);
        }
        self.dirty = true;
    }

    /// Focus `pane` (spec §4.2). The legacy `focused_panel` mirror moves with it
    /// so the pre-rebuild panes, the `Esc` re-focus arm of `events.rs` and the
    /// persisted `ViewState::focused_panel` tag all name the same pane.
    pub(crate) fn set_focus(&mut self, pane: state::FocusPane) {
        self.focus = pane;
        self.focused_panel = pane.legacy();
        self.legacy_view.focus = self.focused_panel;
        self.dirty = true;
    }

    /// Cycle focus around the §4.2 ring, skipping the panes the current mode does
    /// not present, and return the pane that ended up focused. `Tab` walks it
    /// forward and `Shift+Tab` backward (t-011).
    ///
    /// Skipping is what makes `Tab` never a silent no-op (G5): with a mode that
    /// has no Plan/Workers pane the cycle is `Input → Transcript → Input`, and a
    /// focus that was left on a pane the mode removed is pulled back onto a pane
    /// that is actually on screen.
    pub(crate) fn cycle_focus_pane(&mut self, forward: bool) -> state::FocusPane {
        const ORDER: [state::FocusPane; 4] = state::FocusPane::cycle_order();
        let at = ORDER
            .iter()
            .position(|pane| *pane == self.focus)
            .unwrap_or(0);
        for step in 1..=ORDER.len() {
            let index = if forward {
                (at + step) % ORDER.len()
            } else {
                (at + ORDER.len() - step) % ORDER.len()
            };
            if self.view_mode.presents(ORDER[index]) {
                self.set_focus(ORDER[index]);
                return self.focus;
            }
        }
        // Unreachable: the transcript is present in every mode (§4.3), so the
        // ring always has at least one landing pane. The fall-through keeps the
        // focus where it is rather than inventing an absent one.
        self.focus
    }

    /// Turn transcript follow on or off (spec G7: `Ctrl+F`, `/follow`, and the
    /// scroll keys that pull the viewport back off the tail).
    ///
    /// The legacy `chat_auto_scroll` flag is the same decision spelled for the
    /// pre-rebuild scroll and clamp path (`render.rs`, `events.rs`), so it moves
    /// with the flag: one operator decision, one number in the frame.
    pub(crate) fn set_follow(&mut self, follow: bool) {
        self.follow = follow;
        self.chat_auto_scroll = follow;
        self.legacy_view.follow = follow;
        self.dirty = true;
    }

    /// Open the `?`/`F1` keymap + command legend (spec §3, G2).
    ///
    /// The legend is a toggle, so it stays open across a confirm surface drawn
    /// over it — `close_overlay` returns it. §7.3 draws exactly one overlay per
    /// frame, and a pending confirm is the one that wins.
    pub(crate) fn open_help(&mut self) {
        self.help_open = true;
        self.overlay = state::OverlayState::for_surface(self.confirm_open, self.help_open);
        self.dirty = true;
    }

    /// Open the confirm surface: a destructive action awaiting `y`/`n` (spec G3,
    /// §3), and the surface the armed-abort prompt of §8.5 also occupies.
    pub(crate) fn show_confirm(&mut self) {
        self.confirm_open = true;
        self.overlay = state::OverlayState::for_surface(self.confirm_open, self.help_open);
        self.dirty = true;
    }

    /// Close the surface that is on screen — `Esc` with an overlay open closes it
    /// and never arms abort (spec §5, §4.4).
    ///
    /// A confirm on top is dismissed and the legend returns underneath if it was
    /// toggled on; with only the legend open, the legend closes. Nothing here
    /// touches the abort arming itself: dropping an overlay must neither arm nor
    /// disarm (acceptance criterion 16).
    pub(crate) fn close_overlay(&mut self) {
        match self.overlay {
            state::OverlayState::Confirm => {
                self.confirm_open = false;
                if self.help_open {
                    self.open_help();
                    return;
                }
                self.overlay = state::OverlayState::None;
            }
            state::OverlayState::Help => {
                self.help_open = false;
                self.overlay = state::OverlayState::None;
            }
            state::OverlayState::None => {}
        }
        self.dirty = true;
    }

    /// Fold the writes the legacy key path still makes directly into the real
    /// view state, and keep the overlay surface honest with the 3 s abort window.
    ///
    /// `events.rs` owns `show_plan_panel` / `show_subagent_panel` /
    /// `focused_panel` / `chat_auto_scroll` / `confirm_abort` until t-011 rewires
    /// the keys onto the transitions above. Each change is turned into exactly
    /// ONE transition — the mirror records what this renderer last wrote, so a
    /// write the transitions made is never re-adopted and the stored
    /// mode/focus/follow never drift back out of a pane toggle.
    pub(crate) fn adopt_legacy_view_writes(&mut self) {
        if self.show_plan_panel != self.legacy_view.plan_visible
            || self.show_subagent_panel != self.legacy_view.workers_visible
        {
            self.set_view_mode(state::ViewMode::for_panes(
                self.show_plan_panel,
                self.show_subagent_panel,
            ));
        }
        if self.focused_panel != self.legacy_view.focus {
            self.set_focus(state::FocusPane::from_legacy(self.focused_panel));
        }
        if self.chat_auto_scroll != self.legacy_view.follow {
            self.set_follow(self.chat_auto_scroll);
        }
        // The armed-abort window is a wall-clock surface, not a toggle (§8.5).
        // Its rising edge puts the Confirm surface on screen and its falling
        // edge — three seconds later, or a `clear_abort` — takes it back off with
        // no key press at all. Edge-triggered on purpose: a confirm request t-011
        // opens itself must never be closed by an unrelated abort window.
        let armed = self.is_abort_armed();
        if armed != self.legacy_view.abort_armed {
            if armed {
                self.show_confirm();
            } else {
                self.close_overlay();
            }
            self.legacy_view.abort_armed = armed;
        }
    }

    /// Drain pending terminal events and apply render-throttling:
    /// - Force a full re-render when terminal events were handled.
    /// - When state is dirty (new events, tokens, status updates), flush up to 40 FPS (25 ms).
    /// - When an animated spinner is active, flush at 8 FPS (125 ms).
    /// - When completely idle and clean, do NOT re-render (0% CPU idle).
    pub(crate) fn drain_and_throttle_flush(&mut self, blocking: bool) {
        let handled = self.handle_events(blocking);
        // The key path still writes the pre-rebuild view fields directly (t-011
        // moves those keys onto the transition helpers), so every pass through
        // the entry path folds those writes into the real view state before a
        // frame is built. Running it unconditionally, rather than only for a
        // handled event, is also what closes the armed-abort Confirm surface
        // when its 3 s window expires with no key press at all (spec §8.5): the
        // expiry sets `dirty`, so the surface never lingers on screen.
        self.adopt_legacy_view_writes();
        if handled {
            self.dirty = false;
            let _ = self.force_flush();
        } else {
            let active_spinner = self.subagents.iter().any(|s| s.is_active)
                || ACTIVITY_STATUS_KEYWORDS
                    .iter()
                    .any(|k| self.status_line.contains(k));

            if self.dirty {
                if self.last_render.elapsed() >= Duration::from_millis(25) {
                    self.dirty = false;
                    let _ = self.flush();
                }
            } else if active_spinner && self.last_render.elapsed() >= Duration::from_millis(125) {
                let _ = self.flush();
            }
        }
    }
}

impl Default for TuiRenderer {
    fn default() -> Self {
        Self::new()
    }
}

#[inline]
fn estimate_stream_tokens(text: &str) -> usize {
    if text.is_empty() {
        0
    } else if text.len() <= 4 {
        1
    } else {
        text.len().div_ceil(4)
    }
}

/// Upper bound on the log lines one Subagents panel row keeps in the renderer.
///
/// Live log text has exactly one writer — this renderer (`Event::Status` / `ToolCall` /
/// `ToolResult` routing plus [`TuiRenderer::upsert_subagent`]) — while the session loop's
/// rows can only ever carry the lifecycle lines of `helpers::update_subagent_lifecycle`.
/// A merge can therefore only ever *add* text, and a long-running round would grow one
/// row's list without limit. The newest lines win because the detail pane is read from
/// its tail: dropping the oldest lines keeps the activity the operator is watching.
pub(crate) const MAX_SUBAGENT_ROW_LOGS: usize = 200;

/// How many steering instructions the renderer keeps in [`TuiRenderer::steer_traces`].
///
/// The ledger is a *witness* of the bridge's recent transitions, not an archive: the
/// question a pane answers is "what happened to what I just sent", so the newest
/// transitions are the ones that must survive, and an unbounded vector would grow for
/// the whole life of a session. The cap drops the **oldest** entries only.
pub(crate) const MAX_STEER_TRACES: usize = 12;

/// Longest instruction one transcript line quotes before it marks the cut.
const MAX_STEER_QUOTED_CHARS: usize = 160;

/// The one transcript line naming what happened to one steering instruction.
///
/// The label comes from [`components::transcript::STEER_LINE_PREFIX`], which is also what
/// the transcript pane styles on — the renderer writes the sentence, the pane decides how
/// it looks, and both read the same constant so the pairing cannot drift.
fn steer_transcript_line(trace: &SteerTrace) -> String {
    let collapsed = trace
        .text
        .split_whitespace()
        .collect::<Vec<&str>>()
        .join(" ");
    let quoted = if collapsed.chars().count() > MAX_STEER_QUOTED_CHARS {
        let head: String = collapsed.chars().take(MAX_STEER_QUOTED_CHARS).collect();
        format!("{head}{}", theme::ELLIPSIS)
    } else {
        collapsed
    };
    let notice = trace
        .notice_id
        .as_deref()
        .map(|id| format!(" (notice id {id})"))
        .unwrap_or_default();
    format!(
        "{}{} → {}] {quoted} — {}{notice}",
        components::transcript::STEER_LINE_PREFIX,
        trace.stage.label(),
        trace.target,
        trace.reason,
    )
}

/// Merge the renderer's live log lines with the ones an authoritative push carries.
///
/// `logs` needs the same kind of merge guard `thinking` and `content` already have in
/// [`Renderer::set_subagents`]: the loop's rows are a registry projection, the registry
/// read model (`orchestrator::WorkerEntry`) has no log field, and the projection never
/// writes the field — so the incoming list can only ever hold lifecycle lines
/// (`started/completed task …`, `[aborted by user]`, `[cancelled by arbitrator]`). A
/// wholesale replace deletes every streamed line the operator is looking at, which is the
/// "subagent LOG LINES are erased from the Subagents panel" defect.
///
/// Rules, in order:
/// 1. **Monotone adoption.** The incoming list becomes the base only when it is at least
///    as long as the renderer's list. That is the only case in which the loop can
///    genuinely know more, and it is what keeps the pinned contract "the loop owns the log
///    lines and hands them over with the row" intact for a row the renderer opened empty.
/// 2. **The shorter list never loses lines.** Every line the base does not already carry
///    is kept, so the lifecycle lines a longer renderer list never saw survive, and so
///    does the renderer's own streamed tail when the loop's list wins rule 1.
/// 3. **Dedup.** A line present in both lists appears once, so re-pushing the same row
///    (every delegation drain, every 250 ms in-sink pump) can never duplicate it.
/// 4. **Cap.** Only the newest [`MAX_SUBAGENT_ROW_LOGS`] lines survive.
///
/// Deterministic and order-stable: the base keeps its own order and the other list's
/// missing lines follow it in their own order, so draw-tier assertions stay assertable.
pub(crate) fn merge_logs(existing: &[String], incoming: &[String]) -> Vec<String> {
    let incoming_wins = incoming.len() >= existing.len();
    let base = if incoming_wins { incoming } else { existing };
    let other = if incoming_wins { existing } else { incoming };

    let mut merged = base.to_vec();
    for line in other {
        if !merged.contains(line) {
            merged.push(line.clone());
        }
    }
    if merged.len() > MAX_SUBAGENT_ROW_LOGS {
        let stale = merged.len() - MAX_SUBAGENT_ROW_LOGS;
        merged.drain(..stale);
    }
    merged
}

impl Renderer for TuiRenderer {
    fn init(&mut self) -> Result<()> {
        enable_raw_mode()?;
        execute!(
            io::stdout(),
            EnterAlternateScreen,
            PushKeyboardEnhancementFlags(KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES)
        )?;
        let stdout = io::stdout();
        let backend = CrosstermBackend::new(stdout);
        self.terminal = Some(Terminal::new(backend)?);
        // Bracketed paste (SGR mode 2004): without it a multi-line paste arrives as
        // ordinary key events, where every embedded CR parses as KeyCode::Enter and
        // fires one steering command per line. Placed after every fallible step and
        // error-tolerant (`let _ =`): init() propagates failures with `?` and never
        // rolls back modes it has already switched on, so enabling paste last means an
        // aborted startup can never leave 2004 stuck on, and a terminal that rejects
        // the request still starts the app (degrading to the old per-key behaviour).
        // Every successful enable is mirrored by DisableBracketedPaste in shutdown()
        // and leave_alt_screen() so the flag never leaks into the user's shell.
        let _ = execute!(io::stdout(), EnableBracketedPaste);

        let plan = crate::manager::phase::Plan::default();
        if let Ok(Some(content)) = plan.read()
            && !content.trim().is_empty()
        {
            self.plan_content = content;
            self.show_plan_panel = true;
            self.had_active_plan = true;
        }
        // A plan restored from disk is the one pane decision taken before the
        // first frame, so the view mode has to adopt it (a boot with a live plan
        // is M2, spec §4.1) rather than stay on the M1 the constructor chose.
        self.adopt_legacy_view_writes();
        self.terminal_torn_down = false;
        self.force_flush()?;
        Ok(())
    }

    fn on_event(&mut self, event: &Event) {
        self.dirty = true;
        match event {
            Event::TokensIn(count) => {
                self.orchestrator_context_tokens = *count;
                self.tokens_in = self.tokens_in.saturating_add(*count);
            }
            Event::TokensOut(count) => {
                self.tokens_out = self.tokens_out.saturating_add(*count);
            }
            Event::Message(text) => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        sa.content.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
                    self.current_content.push_str(text);
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::SteerResponse(text) => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                // Stream steer arbitrator output as `Marmennill: ` yellow sentences.
                self.steer_sentence_buffer.push_str(text);
                let sentences = extract_complete_sentences(&mut self.steer_sentence_buffer);
                for s in sentences {
                    self.append_steer_sentence(&s);
                }
                if self.chat_auto_scroll {
                    let w = self.chat_width.get();
                    let n = self.estimated_chat_lines(w);
                    let h = self.chat_height.get();
                    self.chat_scroll = n.saturating_sub(h) as u16;
                }
            }
            Event::Thinking(text) => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        sa.thinking.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
                    self.current_thought.push_str(text);
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::SubagentMessage { agent_tag, text } => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                // The tag is a stream spelling, the row is keyed by the effective
                // registry key: resolve first so the text, the thinking flag and
                // the row are all the same object.
                let key = self.resolve_row_key(agent_tag);
                self.subagent_turn_thinking.remove(&key);
                self.subagent_is_thinking.insert(key.clone(), false);
                if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == key) {
                    sa.content.push_str(text);
                    sa.last_activity_at = Some(std::time::Instant::now());
                } else {
                    self.upsert_subagent(&key, true, "running");
                    if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == key) {
                        sa.content.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                }
            }
            Event::SubagentThinking { agent_tag, text } => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                let key = self.resolve_row_key(agent_tag);
                self.subagent_is_thinking.insert(key.clone(), true);
                self.subagent_turn_thinking
                    .entry(key.clone())
                    .or_default()
                    .push_str(text);
                if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == key) {
                    sa.thinking.push_str(text);
                    sa.last_activity_at = Some(std::time::Instant::now());
                } else {
                    self.upsert_subagent(&key, true, "running");
                    if let Some(sa) = self.subagents.iter_mut().find(|s| s.name == key) {
                        sa.thinking.push_str(text);
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                }
            }
            Event::SubagentStreamPause { agent_tag, paused } => {
                // The transition is typed and carries the worker's own stream tag,
                // so no status wording is ever sniffed here. It is a state word on
                // the row, never a log line: the `(Paused)` label and the detail
                // bar read `SubagentDetail::paused`.
                //
                // The session loop already folded the same transition into its
                // authoritative rows (and pushes them through `set_subagents`); this
                // arm keeps a renderer that is driven without that fold — and the
                // frame drawn before that push lands — honest. It deliberately never
                // creates a row: the registry projection owns row existence, so a
                // pause for a worker with no row is a no-op.
                let key = self.resolve_row_key(agent_tag);
                match self.subagents.iter_mut().find(|s| s.name == key) {
                    Some(sa) => {
                        sa.paused = *paused;
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                    None => {
                        tracing::debug!(
                            "stream-pause transition for {key} has no panel row yet; the worker registry owns row existence, so it is skipped"
                        );
                    }
                }
            }
            Event::ToolCall(text) => {
                self.waiting_for_token_since = None;
                let tok_count = estimate_stream_tokens(text);
                self.tokens_out = self.tokens_out.saturating_add(tok_count);
                self.commit_turn_content();
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    self.subagent_turn_thinking.remove(&self.active_agent);
                    self.subagent_is_thinking
                        .insert(self.active_agent.clone(), false);
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        let log_entry = text.clone();
                        if sa.logs.last().map(String::as_str) != Some(log_entry.as_str()) {
                            sa.logs.push(log_entry);
                        }
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
                    self.messages.push(format!("[Tool Call] {text}"));
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::ToolResult(text) => {
                self.waiting_for_token_since = None;
                self.commit_turn_content();
                let is_active_subagent = self.active_agent != "Manager"
                    && self
                        .subagents
                        .iter()
                        .any(|s| s.name == self.active_agent && s.is_active);
                if is_active_subagent {
                    if let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent)
                    {
                        let log_entry = text.clone();
                        if sa.logs.last().map(String::as_str) != Some(log_entry.as_str()) {
                            sa.logs.push(log_entry);
                        }
                        sa.last_activity_at = Some(std::time::Instant::now());
                    }
                } else {
                    if self.active_agent != "Manager" {
                        self.active_agent = "Manager".to_string();
                    }
                    self.messages.push(format!("[Tool Result] {text}"));
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
            }
            Event::Status(text) => {
                if !self.steer_sentence_buffer.trim().is_empty() {
                    let remaining = std::mem::take(&mut self.steer_sentence_buffer);
                    self.append_steer_sentence(remaining.trim());
                }
                if !self.is_abort_armed() {
                    self.status_line = text.lines().next().unwrap_or("").to_string();
                }
                // The elapsed "(n.ns)" counter covers every wait for the backend,
                // including a wait for a concurrency slot: the queue traffic the
                // gate emits (`"queued for a backend slot (n of m) …"`) is exactly
                // the silence that would otherwise read as a stalled turn. A stream
                // parked behind the Steer Arbitrator (`"… — stream paused"`) is the
                // same kind of deliberate silence, so the counter covers it too.
                let is_waiting = self.status_line.contains("Running")
                    || self.status_line.contains("thinking")
                    || self.status_line.contains("calling backend")
                    || self.status_line.contains("Arbitrating")
                    || self.status_line.contains("queued")
                    || self.status_line.contains("paused");
                if is_waiting {
                    if self.waiting_for_token_since.is_none() {
                        self.waiting_for_token_since = Some(std::time::Instant::now());
                    }
                } else if self.status_line.contains("Ready") {
                    self.waiting_for_token_since = None;
                }
                if text.starts_with("[CLI]")
                    || text.starts_with("System Error")
                    || text.starts_with("LLM error")
                {
                    for line in text.lines() {
                        self.messages.push(line.to_string());
                    }
                    // §8.5: keep the cause verbatim. These are exactly the lines the
                    // transcript now carries, so the remembered error and the one `e`
                    // recalls cannot drift apart. A report the loop repeats (the same
                    // failure re-announced on a retry) keeps the first record rather
                    // than restamping it: the buffer marks the edge, not the traffic.
                    if let Some(sticky) = sticky_error_from(text)
                        && self.last_error.as_ref() != Some(&sticky)
                    {
                        self.last_error = Some(sticky);
                    }
                    if self.chat_auto_scroll {
                        let w = self.chat_width.get();
                        let n = self.estimated_chat_lines(w);
                        let h = self.chat_height.get();
                        self.chat_scroll = n.saturating_sub(h) as u16;
                    }
                }
                // Live-track logs for subagents: match by agent name prefix (e.g. "coder-<id>: ...")
                // or fallback to active_agent when multiple specialists run in parallel.
                let mut routed = false;
                for sa in &mut self.subagents {
                    if text.starts_with(&format!("{}:", sa.name))
                        || text.starts_with(&format!("{} ", sa.name))
                        || text.contains(&format!("for {}", sa.name))
                    {
                        let clean = if let Some(stripped) =
                            text.strip_prefix(&format!("{}: ", sa.name))
                        {
                            stripped.to_string()
                        } else if let Some(stripped) = text.strip_prefix(&format!("{}:", sa.name)) {
                            stripped.trim_start().to_string()
                        } else {
                            text.clone()
                        };
                        if sa.logs.last().map(String::as_str) != Some(clean.as_str()) {
                            sa.logs.push(clean);
                        }
                        if text.contains("thinking / calling model")
                            || text.contains("calling backend")
                        {
                            self.subagent_turn_thinking.remove(&sa.name);
                            self.subagent_is_thinking.insert(sa.name.clone(), false);
                        }
                        sa.last_activity_at = Some(std::time::Instant::now());
                        routed = true;
                        break;
                    }
                }
                if !routed
                    && self.active_agent != "Manager"
                    && let Some(sa) = self
                        .subagents
                        .iter_mut()
                        .find(|s| s.name == self.active_agent && s.is_active)
                {
                    let clean = if let Some(stripped) = text.strip_prefix(&format!("{}: ", sa.name))
                    {
                        stripped.to_string()
                    } else if let Some(stripped) = text.strip_prefix(&format!("{}:", sa.name)) {
                        stripped.trim_start().to_string()
                    } else {
                        text.clone()
                    };
                    if sa.logs.last().map(String::as_str) != Some(clean.as_str()) {
                        sa.logs.push(clean);
                    }
                    if text.contains("thinking / calling model") || text.contains("calling backend")
                    {
                        self.subagent_turn_thinking.remove(&sa.name);
                        self.subagent_is_thinking.insert(sa.name.clone(), false);
                    }
                    sa.last_activity_at = Some(std::time::Instant::now());
                }
            }
            Event::Delegation(de) => {
                self.commit_turn_content();
                match de {
                    crate::orchestrator::DelegationEvent::Started { agent, task } => {
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        // The derived spelling is only a search key: the row it
                        // belongs to is the one named by the effective registry
                        // key, which may carry a `-wN` / `#N` handle this event
                        // can never spell.
                        let name = self.resolve_row_key(&name);
                        self.active_agent = name.clone();
                        let t = clean_task.unwrap_or("(no task id)");
                        if let Some(tid) = clean_task {
                            self.active_plan_task = Some(tid.to_string());
                            self.plan_auto_scroll = true;
                            self.show_plan_panel = true;
                        }
                        // Fold into the local subagent list so the panel stays
                        // live even before the session loop pushes the
                        // authoritative list.
                        self.upsert_subagent(&name, true, &format!("started task {t}"));
                        self.subagent_turn_thinking.remove(&name);
                        self.subagent_is_thinking.insert(name.clone(), false);
                        if let Some(s) = self.subagents.iter_mut().find(|s| s.name == name) {
                            s.task_id = clean_task.map(|tid| tid.to_string());
                        }
                        self.status_line = format!("Delegating to specialist {name} for {t}");
                    }
                    crate::orchestrator::DelegationEvent::Completed { agent, task } => {
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        let name = self.resolve_row_key(&name);
                        let t = clean_task.unwrap_or("(no task id)");
                        self.upsert_subagent(&name, false, &format!("completed task {t}"));
                        self.subagent_turn_thinking.remove(&name);
                        self.subagent_is_thinking.remove(&name);
                        self.plan_mtime = None;
                        self.last_plan_check = std::time::Instant::now()
                            .checked_sub(std::time::Duration::from_secs(1))
                            .unwrap_or_else(std::time::Instant::now);

                        let completed_msg = match clean_task {
                            Some(clean) => format!("[{clean}] completed."),
                            None => format!("[{agent}] completed."),
                        };
                        self.messages.push(completed_msg);
                        if self.chat_auto_scroll {
                            let w = self.chat_width.get();
                            let n = self.estimated_chat_lines(w);
                            let h = self.chat_height.get();
                            self.chat_scroll = n.saturating_sub(h) as u16;
                        }
                        if self.active_plan_task.as_deref() == clean_task {
                            self.active_plan_task = self
                                .subagents
                                .iter()
                                .find(|s| s.is_active && s.task_id.is_some() && s.name != name)
                                .and_then(|s| s.task_id.clone());
                        }
                        let remaining_active: Vec<&str> = self
                            .subagents
                            .iter()
                            .filter(|s| s.is_active && s.name != name)
                            .map(|s| s.name.as_str())
                            .collect();
                        if !remaining_active.is_empty() {
                            self.active_agent = remaining_active[0].to_string();
                            self.status_line = format!(
                                "{name} finished {t}. Active: {}",
                                remaining_active.join(", ")
                            );
                        } else {
                            self.active_agent = "Manager".to_string();
                            self.status_line = format!("{name} finished task {t}");
                        }
                    }
                    crate::orchestrator::DelegationEvent::Queued {
                        agent,
                        task,
                        position,
                        of,
                    } => {
                        // Concurrency-slot state inside an already-dispatched
                        // delegation. The row exists by now (its `Started` arm ran
                        // first in the same authoritative stream), so this moves
                        // nothing but the queue field plus the row's activity
                        // stamp: no log line, no status line, no spinner or panel
                        // change — rendering the `(Queued n/m)` label is a later
                        // task and is deliberately not introduced here.
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        let key = self.resolve_row_key(&name);
                        if let Some(s) = self.subagents.iter_mut().find(|s| s.name == key) {
                            s.queue = Some(crate::ui::QueueState {
                                position: *position,
                                of: *of,
                            });
                            s.last_activity_at = Some(std::time::Instant::now());
                        }
                    }
                    crate::orchestrator::DelegationEvent::Running { agent, task } => {
                        // The queued call holds its slot: take the queue state back
                        // down. `is_active`, clocks and selection stay untouched —
                        // a queued delegation was already active, and this arm must
                        // never resurrect a row the lifecycle path closed.
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        let key = self.resolve_row_key(&name);
                        if let Some(s) = self.subagents.iter_mut().find(|s| s.name == key) {
                            s.queue = None;
                            s.last_activity_at = Some(std::time::Instant::now());
                        }
                    }
                    crate::orchestrator::DelegationEvent::Failed {
                        agent,
                        task,
                        reason,
                    } => {
                        let clean_task = task
                            .as_ref()
                            .map(|t| crate::task_id::normalize_task_id_ref(t))
                            .filter(|t| !t.is_empty());
                        let name = match clean_task {
                            Some(t) => format!("{agent}-{t}"),
                            None => format!("{agent}"),
                        };
                        let name = self.resolve_row_key(&name);
                        let t = clean_task.unwrap_or("(no task id)");
                        let log_entry = match reason.as_deref().filter(|r| !r.is_empty()) {
                            Some(r) => format!("failed task {t}: {r}"),
                            None => format!("failed task {t}"),
                        };
                        self.upsert_subagent(&name, false, &log_entry);
                        self.plan_mtime = None;
                        self.last_plan_check = std::time::Instant::now()
                            .checked_sub(std::time::Duration::from_secs(1))
                            .unwrap_or_else(std::time::Instant::now);

                        let failed_msg =
                            match (clean_task, reason.as_deref().filter(|r| !r.is_empty())) {
                                (Some(clean), Some(r)) => format!("[{clean} failed: {r}]"),
                                (Some(clean), None) => format!("[{clean}] failed."),
                                (None, Some(r)) => format!("[{agent} failed: {r}]"),
                                (None, None) => format!("[{agent}] failed."),
                            };
                        self.messages.push(failed_msg);
                        if self.chat_auto_scroll {
                            let w = self.chat_width.get();
                            let n = self.estimated_chat_lines(w);
                            let h = self.chat_height.get();
                            self.chat_scroll = n.saturating_sub(h) as u16;
                        }
                        if self.active_plan_task.as_deref() == clean_task {
                            self.active_plan_task = self
                                .subagents
                                .iter()
                                .find(|s| s.is_active && s.task_id.is_some() && s.name != name)
                                .and_then(|s| s.task_id.clone());
                        }
                        let remaining_active: Vec<&str> = self
                            .subagents
                            .iter()
                            .filter(|s| s.is_active && s.name != name)
                            .map(|s| s.name.as_str())
                            .collect();
                        if !remaining_active.is_empty() {
                            self.active_agent = remaining_active[0].to_string();
                            self.status_line = format!(
                                "{name} failed {t}. Active: {}",
                                remaining_active.join(", ")
                            );
                        } else {
                            self.active_agent = "Manager".to_string();
                            self.status_line = match reason.as_deref().filter(|r| !r.is_empty()) {
                                Some(r) => format!("{name} failed task {t}: {r}"),
                                None => format!("{name} failed task {t}"),
                            };
                        }
                    }
                };
            }
            Event::Done => {
                self.commit_turn_content();
            }
        }
        let handled = self.handle_events(false);
        // Fold whatever the key path wrote behind the API into the real view
        // state before the frame is built (see `adopt_legacy_view_writes`).
        self.adopt_legacy_view_writes();
        if handled || matches!(event, Event::Done) {
            let _ = self.force_flush();
        } else {
            let _ = self.flush();
        }
    }

    fn force_flush(&mut self) -> Result<()> {
        self.render_terminal()
    }

    fn flush(&mut self) -> Result<()> {
        if self.last_render.elapsed() < Duration::from_millis(25) {
            return Ok(());
        }
        self.render_terminal()
    }

    fn poll_input(&mut self) -> Option<String> {
        self.drain_and_throttle_flush(false);
        self.rx.try_recv().ok()
    }

    fn read_input(&mut self) -> Option<String> {
        self.commit_turn_content();
        self.status_line = "Ready".to_string();
        let _ = self.force_flush();
        loop {
            // Esc / Ctrl+C sets `aborted`; return `None` so the session loop
            // stops cleanly instead of hanging while waiting for input.
            if self.aborted() {
                return None;
            }
            self.drain_and_throttle_flush(true);
            if let Ok(line) = self.rx.try_recv() {
                return Some(line);
            }
        }
    }

    fn input_state(&mut self) -> &mut crate::ui::InputState {
        &mut self.input_state
    }

    fn input_state_shared(&self) -> &crate::ui::InputState {
        &self.input_state
    }

    fn request_abort(&mut self) {
        self.input_state.aborted = true;
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.dirty = true;
        crate::orchestrator::cancel_all();
    }

    fn request_user_exit(&mut self) {
        self.input_state.aborted = true;
        self.input_state.user_exit = true;
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.status_line = "Aborted by user.".to_string();
        self.dirty = true;
        crate::orchestrator::cancel_all();
    }

    fn clear_abort(&mut self) {
        self.input_state.aborted = false;
        self.input_state.user_exit = false;
        self.confirm_abort = false;
        self.abort_armed_at = None;
        self.dirty = true;
        crate::orchestrator::reset_cancellation();
    }

    /// The TUI is the renderer that gates destructive commands (spec §5, §6, G3):
    /// `events.rs::arm_destructive_command` puts every destructive slash spelling on
    /// the confirm surface before anything is sent, and the bare words §5 removes are
    /// handed over as ordinary steering text. The session loop therefore reads only
    /// the slash spellings here (`helpers::is_abort_command`); the headless
    /// `RawRenderer` and the test renderers keep the pre-rebuild vocabulary.
    fn gates_destructive_commands(&self) -> bool {
        true
    }

    /// Tear the terminal back down (spec §6.1 entry-path contract).
    ///
    /// Idempotent and panic-safe: every emission is error-tolerant (`let _ =`) —
    /// after a panic hook or a broken pipe there is nobody left to answer an
    /// escape sequence — and a second call is a no-op rather than a second
    /// round-trip to a screen we no longer own. `Drop` calls this, so the normal
    /// path (`shutdown()` at the end of the session loop) and the unwinding path
    /// (renderer dropped by a panic) can both run without double-teardown.
    ///
    /// `DisableMouseCapture` is defensive and asymmetric by history, not by
    /// design: this renderer never emits `EnableMouseCapture`, and the rebuilt UI
    /// takes **no** mouse input (spec §6: the keymap is the whole interface, and
    /// the legacy mouse arms in `events.rs` are retired with the key path). The
    /// disable is kept because a terminal that inherited mouse mode from
    /// somewhere else (a wrapper, a leaked previous process) must still be handed
    /// back to the shell cleanly. Nothing in the UI may assume mouse events
    /// arrive.
    fn shutdown(&mut self) {
        if self.terminal_torn_down {
            return;
        }
        self.terminal_torn_down = true;
        let _ = execute!(
            io::stdout(),
            PopKeyboardEnhancementFlags,
            LeaveAlternateScreen,
            DisableMouseCapture,
            DisableBracketedPaste
        );
        let _ = disable_raw_mode();
    }

    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        // Adopt the session-loop's authoritative list, preserving any live
        // thinking/content already streamed into the local entries.
        let mut activated_name = None;
        for incoming in &subagents {
            match self.subagents.iter_mut().find(|s| s.name == incoming.name) {
                Some(existing) => {
                    let was_inactive = !existing.is_active;
                    existing.is_active = incoming.is_active;
                    if was_inactive && incoming.is_active {
                        activated_name = Some(incoming.name.clone());
                    }
                    // The loop's rows can only ever carry lifecycle lines, so the
                    // incoming list is merged (monotone adoption + dedup + cap)
                    // instead of replacing the renderer's live log list wholesale.
                    existing.logs = merge_logs(&existing.logs, &incoming.logs);
                    // The clocks belong to the registry, not to whoever created the
                    // row: the loop's row carries the entry's own start stamp while
                    // it is live and its finished total once it is not, so a
                    // renderer-created row and the authoritative row for the same
                    // worker can no longer show two different clocks.
                    existing.started_at = incoming.started_at;
                    existing.worked_duration = incoming.worked_duration;
                    if incoming.last_activity_at.is_some() {
                        existing.last_activity_at = incoming.last_activity_at;
                    }
                    if incoming.context_tokens > 0 {
                        existing.context_tokens = incoming.context_tokens;
                    }
                    if existing.prompt.is_empty() && !incoming.prompt.is_empty() {
                        existing.prompt = incoming.prompt.clone();
                    }
                    if incoming.task_id.is_some() {
                        existing.task_id = incoming.task_id.clone();
                    }
                    // Queue state is owned by the session loop's authoritative
                    // list (it is the only writer of the delegation event
                    // stream), so the loop's view always wins here — that is what
                    // keeps a queue row from surviving a completion locally. The
                    // arbitration pause follows the same rule: the loop folds the
                    // typed transition from the preemption handshake into its own
                    // rows, and every loop-side teardown of a row takes the flag
                    // back down, so a `Paused` label can never be kept alive by a
                    // renderer that missed the closing edge.
                    existing.queue = incoming.queue;
                    existing.paused = incoming.paused;
                    // Keep locally-streamed thinking/content if the incoming
                    // entry has none yet (the loop only folds lifecycle events).
                    if !incoming.thinking.is_empty() {
                        existing.thinking = incoming.thinking.clone();
                    }
                    if !incoming.content.is_empty() {
                        existing.content = incoming.content.clone();
                    }
                }
                None => {
                    if incoming.is_active {
                        activated_name = Some(incoming.name.clone());
                    }
                    self.subagents.push(incoming.clone());
                }
            }
        }
        // Drop entries absent from the authoritative list.
        self.subagents
            .retain(|s| subagents.iter().any(|i| i.name == s.name));
        // Plumbing delta D1 (design §9.3): the lifecycle the registry already carries
        // (`status_label`, `implementation_turns`, `validation_rounds`,
        // `latest_validator_feedback`) is mirrored onto the rows here, at the same
        // boundary where the loop's queue/pause state is adopted. This is the only
        // writer of those four fields on the renderer side, and it reads one registry
        // snapshot rather than re-deriving anything from prose.
        fold_worker_lifecycle(&mut self.subagents);
        let any_active = self.subagents.iter().any(|s| s.is_active);
        if any_active {
            self.show_subagent_panel = true;
            if self.active_agent != "Manager"
                && !self
                    .subagents
                    .iter()
                    .any(|s| s.name == self.active_agent && s.is_active)
            {
                let active_names: Vec<&str> = self
                    .subagents
                    .iter()
                    .filter(|s| s.is_active)
                    .map(|s| s.name.as_str())
                    .collect();
                if !active_names.is_empty() {
                    self.active_agent = active_names[0].to_string();
                } else {
                    self.active_agent = "Manager".to_string();
                }
            }
        } else {
            if self.active_agent != "Manager" {
                self.active_agent = "Manager".to_string();
            }
            if self.status_line.contains("Active:")
                || self.status_line.starts_with("Delegating to specialist")
            {
                self.status_line = "Ready".to_string();
            }
            self.active_plan_task = None;
        }
        if let Some(name) = activated_name
            && let Some(idx) = self.subagents.iter().position(|s| s.name == name)
        {
            self.selected_subagent_idx = idx;
        } else if self.selected_subagent_idx >= self.subagents.len() {
            self.selected_subagent_idx = self.subagents.len().saturating_sub(1);
        }
    }

    fn set_thinking_budgets(&mut self, cfg: &crate::config::Config) {
        self.set_thinking_budgets(cfg);
    }

    fn reset_active_agent(&mut self) {
        self.active_agent = "Manager".to_string();
    }

    /// The bridge's steering-instruction projection (spec §3, §8.3; DATA §6 gap 7).
    ///
    /// This is the only writer of [`TuiRenderer::steer_traces`]: a queued, forwarded or
    /// refused instruction reaches the panes because the bridge said so at the statement
    /// that queued, filed or refused it — never because the renderer guessed it from
    /// status prose or counted something of its own.
    fn note_steer(&mut self, trace: &SteerTrace) {
        self.record_steer(trace);
    }

    // ---- session-rehydration hooks ----

    /// CAPTURE the pane model verbatim. This is a pure field read: no event is
    /// replayed, nothing is re-rendered, and no line is regenerated. `&self`
    /// because a capture never mutates the UI.
    fn capture_ui(&self) -> crate::session::format::UiSection {
        let budgets = self
            .thinking_budgets
            .iter()
            .map(|(role, budget)| (role.clone(), *budget))
            .collect();
        crate::session::format::UiSection {
            // The pane vector is copied element-for-element, in order,
            // including embedded newlines and empty or duplicate entries.
            chat_messages: self.messages.clone(),
            status_line: self.status_line.clone(),
            subagent_panels: self
                .subagents
                .iter()
                .map(|detail| {
                    // Projection of `SubagentDetail`, plus the per-turn
                    // thinking state, which lives in the renderer's two maps
                    // (`subagent_turn_thinking`, `subagent_is_thinking`) keyed by
                    // the same `name`, and drives the detail pane's spinner and
                    // thinking bar. `None` distinguishes "no map entry" from
                    // "entry holding an empty string" — the pane draws a
                    // different bar for each, so the distinction is pane state.
                    let mut panel = subagent_panel_from_detail(detail);
                    panel.turn_thinking = self.subagent_turn_thinking.get(&detail.name).cloned();
                    panel.is_thinking = self.subagent_is_thinking.get(&detail.name).copied();
                    panel
                })
                .collect(),
            // The semantic journal is NOT renderer state. The live
            // `UiTranscript` is a local of `ui::session::run_session`, so
            // `capture_ui` cannot honestly fill this field; the committer must
            // take it from the transcript owner.
            journal: Vec::new(),
            composer: crate::session::format::ComposerState {
                input_text: self.input_text.clone(),
                cursor: self.cursor,
                input_draft: self.input_draft.clone(),
                history: self.history.clone(),
                history_index: self.history_index,
            },
            view: crate::session::format::ViewState {
                chat_scroll: self.chat_scroll,
                plan_scroll: self.plan_scroll,
                subagent_scroll: self.subagent_scroll,
                subagent_list_scroll: self.subagent_list_scroll,
                selected_subagent_idx: self.selected_subagent_idx,
                focused_panel: focused_panel_tag(self.focused_panel).to_string(),
                show_thought: self.show_thought,
                show_subagent_panel: self.show_subagent_panel,
                show_plan_panel: self.show_plan_panel,
                chat_auto_scroll: self.chat_auto_scroll,
                subagent_autoscroll: self.subagent_autoscroll,
                plan_auto_scroll: self.plan_auto_scroll,
            },
            // Witness only: `Config` stays the authority; capture states the
            // numbers it holds, and restore never adopts them.
            budgets: crate::session::format::BudgetWitness {
                thinking_budgets: budgets,
                default_thinking_budget: self.default_thinking_budget,
            },

            // ---- plan panel: every byte of the plan box ----
            // `plan_content` is the pane text; `plan_is_archived` and
            // `plan_max_scroll` stay out (derived per frame).
            plan_content: self.plan_content.clone(),
            had_active_plan: self.had_active_plan,
            active_plan_task: self.active_plan_task.clone(),
            // The timer the box actually drew: the local override if it has one,
            // otherwise the process-global plan start the renderer itself falls
            // back to (`total_project_elapsed`).
            plan_start: self.plan_start_instant().map(time_point_from_instant),
            plan_completed: self.plan_completed_instant().map(time_point_from_instant),

            // ---- streaming partials ----
            // The in-flight thinking/answer region of the chat pane. Display
            // only: capture records the bytes, it does not close the turn or
            // invent a transcript record.
            current_thought: self.current_thought.clone(),
            current_content: self.current_content.clone(),

            // ---- agent / notice identity ----
            // `Some(..)` always: a capture knows which agent the status line was
            // crediting. `None` is reserved for "a bundle predates this field".
            active_agent: Some(self.active_agent.clone()),
            steer_sentence_buffer: Some(self.steer_sentence_buffer.clone()),

            // ---- token counters (witness only) ----
            // Recorded, never adopted: `on_event` derives these from THIS
            // process's streams, so adopting them would double-count or erase a
            // running total. `restore_panes` says the drift out loud.
            tokens_in: self.tokens_in as u64,
            tokens_out: self.tokens_out as u64,
            orchestrator_context_tokens: self.orchestrator_context_tokens as u64,

            // ---- abort arming (display-only witness) ----
            // The armed window stays out of the live model: a 3-second armed
            // window has no meaning after a restart, and nothing re-arms it.
            // What IS recorded here is that the screen was showing the arming
            // prompt, and when: `is_abort_armed` already applies the 3-second
            // rule, so `true` means "the prompt was genuinely on screen at
            // capture instant".
            abort_armed: self.is_abort_armed(),
            abort_armed_at: self.abort_armed_at.map(time_point_from_instant),
        }
    }

    /// REHYDRATE the panes. Assigns wholesale — never appends, never
    /// regenerates a line from a journal record — then lets the renderer's own
    /// sync/clamp paths do what a resize already does.
    fn restore_panes(&mut self, section: &crate::session::format::UiSection) {
        // 1. Display authority first (fixed order of application).
        self.messages = section.chat_messages.clone();
        self.status_line = section.status_line.clone();
        self.subagents = section
            .subagent_panels
            .iter()
            .map(subagent_detail_from_panel)
            .collect();
        // The per-turn thinking state comes back with its own panels, so the
        // detail pane's spinner / "thinking n/budget" bar is in the state the
        // screen showed. An absent entry stays absent — the pane's fallback then
        // reads `thinking`, exactly as it does without a restore.
        self.subagent_turn_thinking = section
            .subagent_panels
            .iter()
            .filter_map(|panel| {
                panel
                    .turn_thinking
                    .clone()
                    .map(|text| (panel.name.clone(), text))
            })
            .collect();
        self.subagent_is_thinking = section
            .subagent_panels
            .iter()
            .filter_map(|panel| panel.is_thinking.map(|value| (panel.name.clone(), value)))
            .collect();

        // The plan box is pane text too: its content, and the text it flags as
        // in flight. An EMPTY stored plan text means "this bundle predates the
        // field" (`#[serde(default)]`), not "the box was blank": the plan pane is
        // disk-backed and a fresh process fills it at `init`, so assigning an
        // empty string would blank a pane that was never blank and force an
        // immediate reload. One predicate covers the whole plan group.
        let stores_plan_pane = !section.plan_content.is_empty();
        if stores_plan_pane {
            self.plan_content = section.plan_content.clone();
        }

        // The in-flight streaming region: the half-streamed thinking and answer
        // text come back as DISPLAY bytes. Nothing here resumes the provider
        // request or fabricates a finished transcript record; if the next turn
        // completes, the existing flush path (`events.rs`) folds these bytes into
        // the pane the same way it would have in the previous process.
        self.current_thought = section.current_thought.clone();
        self.current_content = section.current_content.clone();

        // Agent identity and the steer-sentence half-buffer. `None` = the bundle
        // predates the field: keep what this process derived at boot rather than
        // inventing `"Manager"`.
        if let Some(agent) = section
            .active_agent
            .as_ref()
            .filter(|agent| !agent.is_empty())
        {
            self.active_agent = agent.clone();
        }
        if let Some(buffer) = section.steer_sentence_buffer.clone() {
            self.steer_sentence_buffer = buffer;
        }

        // 2. Composer: assign text + caret, then rebuild the widget through the
        // existing sync path instead of hand-building a `TextArea`. The
        // textarea's undo/redo stack and in-widget selection anchors are not
        // restorable (ratatui-textarea internals).
        self.input_text = section.composer.input_text.clone();
        self.input_draft = section.composer.input_draft.clone();
        self.history = section.composer.history.clone();
        self.history_index = section.composer.history_index;
        self.cursor = section.composer.cursor;
        self.sanitize_cursor();
        self.ensure_textarea_in_sync();
        let caret = self.cursor;
        self.place_textarea_caret_at_byte(caret);

        // 3. Toggles and auto-scroll flags before the offsets.
        self.show_thought = section.view.show_thought;
        self.show_subagent_panel = section.view.show_subagent_panel;
        self.chat_auto_scroll = section.view.chat_auto_scroll;
        self.subagent_autoscroll = section.view.subagent_autoscroll;
        self.plan_auto_scroll = section.view.plan_auto_scroll;
        if stores_plan_pane {
            self.show_plan_panel = section.view.show_plan_panel;
            self.had_active_plan = section.had_active_plan;
            self.active_plan_task = section.active_plan_task.clone();
        }
        // An out-of-set spelling is a loader refusal, not something the
        // renderer invents: keep the current focus in that case.
        self.focused_panel =
            focused_panel_from_tag(&section.view.focused_panel).unwrap_or(self.focused_panel);

        // 3b. The real view state (spec §4.1, §4.2, §10 item 18). The stored
        // `ViewState` only carries the pre-rebuild spellings — the pane toggles,
        // a focus tag and the auto-scroll flag — so those are read back through
        // the same transition helpers the keys use, which keeps the invariants
        // (`mode` ⇔ pane inventory, `focus` ⇔ the persisted tag, `follow` ⇔ the
        // scroll flag) true for a restored session exactly as they are for a live
        // one. Two documented losses inside that mapping: the tag set has no
        // Composer entry, so a Composer focus comes back as the Transcript
        // (`FocusPane::from_legacy`), and the persisted pair of toggles cannot
        // distinguish a mode the operator left with a pane closed on purpose from
        // one the toggles put there — which is the same ambiguity the pair always
        // had. Neither is invented here.
        //
        // The focus is read out of the tag FIRST because a restore never repairs
        // one: `set_view_mode` pulls a focus off a pane its new mode removed (an
        // absent pane is never drawn or hit-tested, §2.4.1), but a bundle may
        // legitimately persist a tag for a pane its toggles leave closed, and the
        // loader rule for a focus the file states is to keep it verbatim rather
        // than invent another one. So the incoming focus is captured, the mode is
        // applied, and the captured focus is re-asserted last.
        let restored_focus = state::FocusPane::from_legacy(self.focused_panel);
        self.set_view_mode(state::ViewMode::for_panes(
            self.show_plan_panel,
            self.show_subagent_panel,
        ));
        self.set_focus(restored_focus);
        self.set_follow(self.chat_auto_scroll);
        // A Confirm surface belongs to the 3 s abort window, and that window is
        // never re-armed by a restore (step 5 below). If this process was showing
        // one when the restore was applied, the surface goes away with it — while
        // a Help legend the operator opened stays on screen. Resetting the abort
        // mirror in the same breath keeps the adoption bridge from mistaking the
        // restore for the window's falling edge and closing the legend too.
        if self.overlay == state::OverlayState::Confirm {
            self.close_overlay();
        }
        self.legacy_view.abort_armed = false;

        // 4. Scroll offsets and selection, verbatim; the live clamp re-checks
        // them against the freshly restored content.
        self.chat_scroll = section.view.chat_scroll;
        self.plan_scroll = section.view.plan_scroll;
        self.subagent_scroll = section.view.subagent_scroll;
        self.subagent_list_scroll = section.view.subagent_list_scroll;
        self.selected_subagent_idx = section.view.selected_subagent_idx;

        // 4b. Time projections. `to_instant()` is total: a hostile
        // `elapsed_secs`/wall pair degrades to "now", it cannot panic and cannot
        // produce a negative duration. The plan timer is only ever a display
        // timer (`total_project_elapsed`).
        self.plan_start_time = section.plan_start.as_ref().map(|point| point.to_instant());
        self.plan_completed_time = section
            .plan_completed
            .as_ref()
            .map(|point| point.to_instant());

        // 5. Witnesses: recorded, never adopted (the rule for `budgets`, the
        // token counters and abort arming alike). Said out loud in the same shape
        // `announce_restore` uses, then logged — a silent version is forbidden,
        // and `token_witness_warnings` is public so the loader can fold the
        // identical sentence into `RepairReport::warnings`.
        for warning in self.token_witness_warnings(section) {
            self.messages
                .push(format!("[CLI] Restore warning: {warning}"));
            tracing::warn!("session restore (ui.json): {warning}");
        }
        // A 3-second confirm-abort window has no meaning in a new process, and
        // nothing re-arms it. The arming is therefore NOT restored: the live
        // renderer is left unarmed, and the stored `true` becomes a loud
        // statement instead. Silently re-arming would let one keypress abort a
        // session the user never asked to abort, and silently dropping it would
        // hide why the pane says "Abort armed. Press ESC again to abort."
        self.confirm_abort = false;
        self.abort_armed_at = None;
        if section.abort_armed {
            let detail = "confirm-abort was armed when the session was captured; \
                          the armed window is not re-armed after a restart, so it was \
                          dropped — press the abort key again if you still want to abort";
            self.messages.push(format!(
                "[CLI] Not restorable after restart [Other] session: {detail}"
            ));
            tracing::warn!("session restore (ui.json): {detail}");
        }

        // 6. Derived state is never restored, and the line/render caches are
        // keyed by message count, so a wholesale replace MUST invalidate them or
        // the first frame would draw stale bytes.
        self.invalidate_pane_caches();
        self.clamp_scrolls();
        // The clamp above MAY move a restored offset (a stored 9000 cannot
        // survive a 40-line pane), and that move is only legal when a sentence
        // names the offset AND both values. The sentence exists —
        // `clamp_warning_sentences` below, in the `token_witness_warnings` shape
        // — and `ui::session::boot` folds it into `RepairReport::warnings` BEFORE
        // `announce_restore`, which draws it into the panes and logs it. It is
        // deliberately not pushed into `self.messages` here: the number that must
        // be named is the one the renderer ends on (a later selection clamp in
        // `set_subagents` can move the index again), and drawing it twice would
        // make one moved offset read as two disagreements.
        self.dirty = true;
    }

    /// Surface the restore report out loud: a status line, a chat line per loss,
    /// and a `tracing::warn!` — never a `let _` swallow. The third destination,
    /// the journaled `UiRecord::Status`, is written by the transcript owner
    /// (`ui::session`), which is the only place that holds a `UiTranscript`.
    fn announce_restore(&mut self, report: &crate::session::format::RepairReport) {
        for notice in &report.notices {
            self.messages.push(format!(
                "[CLI] Not restorable after restart [{:?}] {}: {}",
                notice.kind, notice.subject, notice.detail
            ));
        }
        for warning in &report.warnings {
            self.messages
                .push(format!("[CLI] Restore warning: {warning}"));
        }
        for counts in &report.worker_counts {
            self.messages.push(format!(
                "[CLI] Worker {} history normalized: {} -> {} message(s) ({})",
                counts.key,
                counts.before,
                counts.after,
                if counts.applied_rules.is_empty() {
                    "no rule applied".to_string()
                } else {
                    counts.applied_rules.join(", ")
                }
            ));
        }
        self.status_line = format!(
            "Session restored: {} pane line(s), {} worker(s){}",
            report.ui_lines_restored,
            report.workers_restored,
            if report.notices.is_empty() {
                String::new()
            } else {
                format!(" — {} capability(s) lost", report.notices.len())
            }
        );
        self.dirty = true;

        tracing::warn!(
            pins_verified = report.pins_verified,
            pinned_prefix_end_expected = ?report.pinned_prefix_end_expected,
            pinned_prefix_end_actual = ?report.pinned_prefix_end_actual,
            orphan_tool_results_pruned = report.orphan_tool_results_pruned,
            aborted_results_synthesized = report.aborted_results_synthesized,
            tool_results_regrouped = report.tool_results_regrouped,
            normalization_passes = report.normalization_passes,
            normalization_stable = report.normalization_stable,
            manager_messages_before = report.manager_messages_before,
            manager_messages_after = report.manager_messages_after,
            ui_lines_restored = report.ui_lines_restored,
            workers_restored = report.workers_restored,
            workers_unrecoverable = ?report.workers_unrecoverable,
            completed_dropped_by_ring_cap = report.completed_dropped_by_ring_cap,
            notices = report.notices.len(),
            warnings = report.warnings.len(),
            "session restore: repair report surfaced to the UI"
        );
    }
}

/// The persisted spelling of [`FocusedPanel`] (`"chat" | "plan" |
/// "subagents"`, the same set as `session::format::FOCUSED_PANELS`).
fn focused_panel_tag(panel: FocusedPanel) -> &'static str {
    match panel {
        FocusedPanel::Chat => "chat",
        FocusedPanel::Plan => "plan",
        FocusedPanel::Subagents => "subagents",
    }
}

/// Inverse of [`focused_panel_tag`]; `None` for an out-of-set spelling, which
/// the loader refuses rather than repairs.
fn focused_panel_from_tag(tag: &str) -> Option<FocusedPanel> {
    match tag {
        "chat" => Some(FocusedPanel::Chat),
        "plan" => Some(FocusedPanel::Plan),
        "subagents" => Some(FocusedPanel::Subagents),
        _ => None,
    }
}

/// Project one live [`SubagentDetail`] onto its durable mirror.
/// `worked_duration_secs` is carried as an explicit number, never re-derived
/// from the two re-anchored instants.
fn subagent_panel_from_detail(detail: &SubagentDetail) -> crate::session::format::SubagentPanel {
    crate::session::format::SubagentPanel {
        name: detail.name.clone(),
        task_id: detail.task_id.clone(),
        prompt: detail.prompt.clone(),
        logs: detail.logs.clone(),
        thinking: detail.thinking.clone(),
        content: detail.content.clone(),
        is_active: detail.is_active,
        context_tokens: detail.context_tokens,
        worked_duration_secs: detail.worked_duration.as_secs_f64(),
        // Projected through [`time_point_from_instant`].
        started_at: detail.started_at.map(time_point_from_instant),
        last_activity_at: detail.last_activity_at.map(time_point_from_instant),
        // This function projects [`SubagentDetail`] and nothing else, so the two
        // per-turn thinking fields are left empty here; `capture_ui` overlays
        // them from the renderer's maps, keyed by the same `name`.
        turn_thinking: None,
        is_thinking: None,
        // Plumbing delta D2 (design §9.4): the slot-queue pair and the arbitration
        // pause ARE persisted now, so a restore puts the row back in the state the
        // screen was actually showing instead of silently downgrading a
        // `(Queued 2/3)` or `(Paused)` row to "idle". Both are `#[serde(default)]`
        // on the way in, so a bundle written before D2 still loads.
        queue: detail
            .queue
            .map(|queue| crate::session::format::QueuePanel {
                position: queue.position,
                of: queue.of,
            }),
        paused: detail.paused,
    }
}

/// Project a live `Instant` onto the durable `TimePoint` pair.
///
/// [`SubagentDetail`] stores bare `Instant`s with **no** wall companion, and
/// `TimePoint::to_instant()` re-anchors from the wall half — so sampling the wall
/// clock *now* would place every stored instant at capture time and lose its age.
/// The wall half is therefore derived from the monotonic age: both halves are
/// sampled in one step, `age` comes from the monotonic clock and is exact within
/// this process, so `wall = now_wall - age` is where that instant actually
/// happened.
///
/// Honest limit: an instant from **before** this process's anchor cannot carry a
/// negative `elapsed_secs`, and `from_instant_with_wall` floors a wall clock older
/// than the anchor, so such an instant re-anchors to roughly "now". A subagent
/// never predates the process, and the loss is asserted by
/// `an_instant_older_than_the_process_anchor_honestly_loses_its_age`.
fn time_point_from_instant(instant: std::time::Instant) -> crate::session::format::TimePoint {
    let age = std::time::Instant::now().saturating_duration_since(instant);
    let age_delta = chrono::Duration::from_std(age).unwrap_or_else(|_| chrono::Duration::zero());
    crate::session::format::TimePoint::from_instant_with_wall(
        instant,
        chrono::Local::now() - age_delta,
    )
}

/// The inverse projection: a stored panel becomes the live model the subagent
/// panes draw, byte-for-byte for every pane string. The two per-turn thinking
/// fields are NOT part of [`SubagentDetail`]; `restore_panes` re-installs them
/// into the renderer's `subagent_turn_thinking` / `subagent_is_thinking` maps.
fn subagent_detail_from_panel(panel: &crate::session::format::SubagentPanel) -> SubagentDetail {
    SubagentDetail {
        name: panel.name.clone(),
        task_id: panel.task_id.clone(),
        prompt: panel.prompt.clone(),
        started_at: panel.started_at.as_ref().map(|point| point.to_instant()),
        worked_duration: duration_from_secs(panel.worked_duration_secs),
        last_activity_at: panel
            .last_activity_at
            .as_ref()
            .map(|point| point.to_instant()),
        logs: panel.logs.clone(),
        thinking: panel.thinking.clone(),
        content: panel.content.clone(),
        is_active: panel.is_active,
        context_tokens: panel.context_tokens,
        // Both slot states come back from the bundle now. Before plumbing delta D2
        // (design §9.4) neither was persisted at all: `queue` belongs to a live
        // delegation's wait in a pool ledger that exists only inside this process, and
        // `paused` to a stream the preemption handshake is holding right now, so a
        // journaled pane recorded a round that was interrupted and the row came back
        // downgraded to idle — a `(Queued 2/3)` or `(Paused)` row lost its state for as
        // long as the pool stayed quiet, and only a fresh `Event::SubagentStreamPause`
        // or a fresh queue event could put it back (fix #3 made the queue label come
        // back by itself once the pool reported a rank, because the projection mirrors
        // `orchestrator::WorkerEntry::queue_position`).
        //
        // D2 closes that window: a panel captured while parked or queued restores as
        // parked or queued. A rank the registry knows about still wins on the next
        // projection, and a panel written before D2 has no such fields, so it restores
        // exactly as it did before — never an invented position.
        queue: panel.queue.map(|queue| crate::ui::QueueState {
            position: queue.position,
            of: queue.of,
        }),
        paused: panel.paused,
        // The D1 lifecycle fields are deliberately NOT persisted: the worker
        // registry is their only source, and a restored row re-derives them from
        // the live entry behind its key. Restoring prose from a bundle would let a
        // label outlive the worker that produced it.
        status_label: None,
        implementation_turns: 0,
        validation_rounds: 0,
        latest_validator_feedback: None,
    }
}

/// `Duration::from_secs_f64` panics on a negative/NaN/infinite input, so a
/// hostile or corrupt number degrades to zero instead of taking the UI down.
fn duration_from_secs(secs: f64) -> Duration {
    if secs.is_finite() && secs > 0.0 {
        Duration::from_secs_f64(secs)
    } else {
        Duration::ZERO
    }
}

/// The scroll-clamp notice: one machine-readable line per restored offset whose
/// stored value differs from the live one.
///
/// This is the ONLY producer of that sentence, so a report and the log line that
/// accompanies it can never phrase the same disagreement two different ways. The
/// boot wiring in `ui::session` folds it into `RepairReport::warnings` through
/// that module's `scroll_clamp_warnings` helper, which compares the two
/// `UiSection`s (boot holds only a `&mut dyn Renderer`, so it reads the live
/// offsets from a re-capture rather than off the renderer).
pub(crate) fn clamp_warning_sentences(moved: &[(&str, u64, u64)]) -> Vec<String> {
    moved
        .iter()
        .copied()
        .filter(|(_, stored, live)| stored != live)
        .map(|(field, stored, live)| {
            format!(
                "{field} clamped by the live geometry: {stored} -> {live}: the stored value does \
                 not fit the restored content, and the live value is what the panes show now"
            )
        })
        .collect()
}

impl TuiRenderer {
    /// Move the widget caret to a restored byte offset — the inverse of
    /// `sync_input_from_textarea`'s (row, col) → byte mapping, so the caret is
    /// visible where the user left it.
    fn place_textarea_caret_at_byte(&mut self, byte: usize) {
        let byte = byte.min(self.input_text.len());
        let mut row = 0usize;
        let mut line_start = 0usize;
        let mut idx = 0usize;
        while idx < byte {
            if self.input_text.as_bytes()[idx] == b'\n' {
                row += 1;
                line_start = idx + 1;
            }
            idx += 1;
        }
        let line = self.textarea.lines().get(row).cloned().unwrap_or_default();
        let col_bytes = (byte - line_start).min(line.len());
        // `byte` is on a char boundary (loader precondition + `sanitize_cursor`),
        // so the slice is safe; the widget counts columns in chars.
        let col = line[..col_bytes].chars().count();
        self.textarea
            .move_cursor(ratatui_textarea::CursorMove::Jump(row as u16, col as u16));
    }

    /// The token counters are a **witness**, never an adoption: that is the rule
    /// already governing `BudgetWitness`, extended to `tokens_in`, `tokens_out`
    /// and `orchestrator_context_tokens`. `on_event` derives them from THIS
    /// process's streams, so adopting stored numbers would double-count a running
    /// total or erase one that has not started.
    ///
    /// One sentence per disagreeing counter, phrased for
    /// [`crate::session::format::RepairReport::warnings`] so the loader can push
    /// them in verbatim; `restore_panes` emits the identical sentence to the pane
    /// and the log, because a silent version is forbidden.
    pub fn token_witness_warnings(
        &self,
        section: &crate::session::format::UiSection,
    ) -> Vec<String> {
        let mut out = Vec::new();
        for (field, stored, live) in [
            ("tokens_in", section.tokens_in, self.tokens_in as u64),
            ("tokens_out", section.tokens_out, self.tokens_out as u64),
            (
                "orchestrator_context_tokens",
                section.orchestrator_context_tokens,
                self.orchestrator_context_tokens as u64,
            ),
        ] {
            if stored != live {
                out.push(format!(
                    "{field} witness {stored} != this process's live {live}: kept the live \
                     counter (the streams own it) and reported the difference instead"
                ));
            }
        }
        out
    }

    /// Drop every derived chat cache so the next frame is rebuilt from the
    /// restored bytes: derived caches are never stored, and a frame must never
    /// draw a line that was not stored.
    fn invalidate_pane_caches(&self) {
        self.cached_rendered_messages.borrow_mut().clear();
        self.cached_message_lines.borrow_mut().clear();
        self.cached_chat_width.set(0);
        self.cached_total_message_lines.set(0);
        self.cached_show_thought.set(!self.show_thought);
        self.cached_rendered_show_thought.set(!self.show_thought);
    }
}

impl Drop for TuiRenderer {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// Leave the alternate screen (best-effort, from the panic hook).
///
/// The panic path bypasses [`Renderer::shutdown`]: the hook runs while the
/// renderer is being unwound, so this is a free function over the global terminal
/// mode and deliberately touches no renderer state. It mirrors every mode `init()`
/// switched on — including bracketed paste, so mode 2004 never leaks into the
/// user's shell — and disables raw mode first so a terminal left in raw mode
/// cannot eat the rest of the panic output.
///
/// Idempotency: the `Drop` of the renderer runs `shutdown()` after this, so the
/// same five sequences are written twice on the unwinding path. That is harmless
/// by construction — every one of them is a reset (`Pop…`, `LeaveAlternateScreen`,
/// `DisableMouseCapture`, `DisableBracketedPaste`, raw mode off), not a toggle —
/// and it is what makes the teardown safe to run from a hook, from `Drop` and from
/// the session loop in any order.
pub fn leave_alt_screen() -> std::io::Result<()> {
    let _ = disable_raw_mode();
    // Mirror every mode init() switched on, including bracketed paste, because the
    // panic path bypasses shutdown() and must not leak mode 2004 into the user's shell.
    execute!(
        io::stdout(),
        PopKeyboardEnhancementFlags,
        LeaveAlternateScreen,
        DisableMouseCapture,
        DisableBracketedPaste
    )
}

/// Run the interactive TUI session using the CLI's optional initial prompt.
pub async fn run(
    cfg: &Config,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
) -> Result<()> {
    let mut renderer = TuiRenderer::new();
    super::run_session(cfg, &mut renderer, initial, manager).await
}

/// Returns `true` for explicit exit/abort commands.
pub(crate) fn is_abort(line: &str) -> bool {
    crate::ui::helpers::is_abort_command(line)
}
