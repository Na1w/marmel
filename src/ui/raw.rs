//! Headless (raw) streaming mode.
//!
//! This renderer streams agent events directly to stdout in a pipe-friendly
//! way (one logical unit per line), so `marmel --raw "explain main.rs"` can be
//! captured by scripts. It never enters raw mode or the alternate screen, so
//! the terminal is always left in a sane state.

use super::{Event, InputState, Renderer, chunk_utf8};
use crate::agents::Agent;
use crate::config::Config;
use crate::markers::MARKER_FAILED;
use crate::orchestrator::OrchestratorManager;
use anyhow::Result;
#[cfg(unix)]
use std::io::IsTerminal;
use std::io::Write;
use std::sync::Arc;

/// Run a headless session using the CLI's optional initial prompt.
pub async fn run(
    cfg: &Config,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
) -> Result<()> {
    let mut renderer = RawRenderer::new();
    super::run_session(cfg, &mut renderer, initial, manager).await
}

/// Restore the terminal state. In raw mode we never modify the terminal, so
/// this is a safe no-op (kept for the panic hook / parity with the TUI).
pub fn restore() -> std::io::Result<()> {
    #[cfg(unix)]
    {
        if std::io::stdout().is_terminal() {
            // We never enter raw mode in this renderer, but we defensively
            // disable it in case a prior TUI session left it enabled.
            let _ = crossterm::terminal::disable_raw_mode();
        }
    }
    Ok(())
}

/// A minimal renderer that prints events to stdout in a stable, pipe-friendly
/// format. Each event becomes a labelled line:
/// `[assistant] <text>`, `[thinking] <text>`, `[tool] name(args)`, `[status] …`.
pub struct RawRenderer {
    /// Buffered lines awaiting a flush.
    buffer: Vec<u8>,
    /// Abort / user-exit flags (the `Renderer` trait's abort surface).
    input_state: InputState,
}

impl RawRenderer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            input_state: InputState::default(),
        }
    }

    fn push_line(&mut self, label: &str, text: &str) {
        // Emit one line per chunk so long streams stay parseable by scripts.
        for chunk in chunk_utf8(text, 512) {
            let _ = writeln!(self.buffer, "[{label}] {chunk}");
        }
    }
}

/// The `delegation` line raw mode prints for a failed delegation.
///
/// The verdict word is the marker owner's constant
/// ([`crate::markers::MARKER_FAILED`]) rather than a hand-typed literal, so the
/// vocabulary has one owner. The rendered bytes are unchanged — `FAILED` is
/// followed by exactly two spaces to column-align with `STARTED →` / `DONE`.
fn delegation_failed_line(agent: Agent, task: Option<&str>, reason: Option<&str>) -> String {
    let t = task.unwrap_or("(no task id)");
    let reason_suffix = reason
        .filter(|reason| !reason.is_empty())
        .map(|reason| format!(": {reason}"))
        .unwrap_or_default();
    format!("{MARKER_FAILED}  {agent} on {t}{reason_suffix}")
}

impl Default for RawRenderer {
    fn default() -> Self {
        Self::new()
    }
}

impl Renderer for RawRenderer {
    fn init(&mut self) -> Result<()> {
        // Nothing to initialise: raw mode writes straight to stdout.
        Ok(())
    }

    fn on_event(&mut self, event: &Event) {
        match event {
            Event::Message(text) => self.push_line("assistant", text),
            Event::SteerResponse(text) => self.push_line("steer", text),
            Event::Thinking(text) => self.push_line("thinking", text),
            Event::SubagentMessage { agent_tag, text } => {
                self.push_line(&format!("{agent_tag}:content"), text);
            }
            Event::SubagentThinking { agent_tag, text } => {
                self.push_line(&format!("{agent_tag}:thinking"), text);
            }
            Event::ToolCall(text) => self.push_line("tool", text),
            Event::ToolResult(text) => self.push_line("tool-result", text),
            Event::Status(text) => self.push_line("status", text),
            Event::Delegation(de) => match de {
                crate::orchestrator::DelegationEvent::Started { agent, task } => {
                    let t = task.as_deref().unwrap_or("(no task id)");
                    self.push_line("delegation", &format!("STARTED → {agent} on {t}"));
                }
                // Slot transitions inside a live delegation stay silent here: queue
                // state is Subagents-panel state, and printing it in raw mode would
                // change headless output, which this change deliberately does not
                // touch.
                crate::orchestrator::DelegationEvent::Queued { .. }
                | crate::orchestrator::DelegationEvent::Running { .. } => {}
                crate::orchestrator::DelegationEvent::Completed { agent, task } => {
                    let t = task.as_deref().unwrap_or("(no task id)");
                    self.push_line("delegation", &format!("DONE    {agent} on {t}"));
                }
                crate::orchestrator::DelegationEvent::Failed {
                    agent,
                    task,
                    reason,
                } => {
                    self.push_line(
                        "delegation",
                        &delegation_failed_line(*agent, task.as_deref(), reason.as_deref()),
                    );
                }
            },
            Event::Done => {
                let _ = writeln!(self.buffer, "[done]");
            }
            Event::TokensIn(_) | Event::TokensOut(_) => {}
        }
        let _ = self.flush();
    }

    fn flush(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let mut out = std::io::stdout().lock();
        out.write_all(&self.buffer)?;
        out.flush()?;
        self.buffer.clear();
        Ok(())
    }

    // `poll_input` / `read_input` use the trait defaults (no interactive
    // stdin in headless mode); the abort-flag surface uses the trait
    // defaults backed by `input_state`, except `request_user_exit`, which
    // additionally cancels all active work.

    fn input_state(&mut self) -> &mut InputState {
        &mut self.input_state
    }

    fn input_state_shared(&self) -> &InputState {
        &self.input_state
    }

    fn request_user_exit(&mut self) {
        self.input_state.aborted = true;
        self.input_state.user_exit = true;
        crate::orchestrator::cancel_all();
    }

    fn shutdown(&mut self) {
        let _ = self.flush();
    }

    // ---- session-rehydration hooks ----

    /// Raw mode keeps **no pane model**, so an honest capture claims **nothing**:
    /// there is no chat pane (events go straight to stdout, which is not readable
    /// back), no status line, no composer (`poll_input`/`read_input` are the trait
    /// defaults), no scroll offsets, no selection and no subagent panels — only
    /// `buffer` (bytes pending a flush) and the abort flags. The empty section
    /// returned is a *loadable* one (`UiSection::structural_violations()` is empty
    /// for it), and every extra pane group — plan box, streaming partials, agent
    /// identity, token counters, abort-arming window — is unclaimed at its serde
    /// default for the same reason.
    fn capture_ui(&self) -> crate::session::format::UiSection {
        crate::session::format::UiSection::default()
    }

    /// Raw mode has no panes to rehydrate, so the honest restore is a **replay of
    /// the stored pane text into the output stream**: one labelled line per stored
    /// entry, in stored order, byte-verbatim, each through `push_line`
    /// (`chunk_utf8`) so no multi-byte character is split and nothing is written
    /// to stdout behind the buffer. `[done]` is never re-emitted, so the
    /// session-loop termination contract and the abort / user-exit flags stand.
    /// What raw mode cannot represent — composer, caret, scroll offsets,
    /// selection, subagent panels, counters, and the plan / streaming / identity /
    /// token / abort groups (see the doc on [`RawRenderer`]) — is named on the
    /// stream (`[restore]` header plus one `[restore-skipped]` line per non-empty
    /// group) instead of being dropped silently.
    fn restore_panes(&mut self, section: &crate::session::format::UiSection) {
        self.push_line(
            "restore",
            &format!(
                "raw mode has no panes: replaying {} stored pane line(s); \
                 composer/view/subagents/budgets/plan/streaming/identity/tokens/abort \
                 fields are not representable here",
                section.chat_messages.len()
            ),
        );
        for line in &section.chat_messages {
            if line.is_empty() {
                // An empty pane line is legal content that SHALL survive:
                // `push_line` would skip it (`chunk_utf8("")` yields no chunk),
                // so it is spelled here as its own labelled line.
                let _ = writeln!(self.buffer, "[restored]");
                continue;
            }
            // The stored string is echoed verbatim; it is never re-rendered.
            self.push_line("restored", line);
        }
        for panel in &section.subagent_panels {
            self.push_line(
                "restore-skipped",
                &format!(
                    "subagent {} ({} log line(s)) has no raw-mode pane",
                    panel.name,
                    panel.logs.len()
                ),
            );
        }
        // Raw mode has no pane for any of these groups, so it names each one on
        // the stream instead of dropping it silently.
        if !section.plan_content.is_empty() {
            self.push_line(
                "restore-skipped",
                &format!(
                    "plan pane ({} byte(s) of plan text, panel visible: {}, in-flight task: {}) \
                     has no raw-mode pane",
                    section.plan_content.len(),
                    section.view.show_plan_panel,
                    section.active_plan_task.as_deref().unwrap_or("none")
                ),
            );
        }
        if !section.current_thought.is_empty() || !section.current_content.is_empty() {
            self.push_line(
                "restore-skipped",
                &format!(
                    "in-flight streaming partials ({} thought byte(s), {} content byte(s)) \
                     have no raw-mode pane",
                    section.current_thought.len(),
                    section.current_content.len()
                ),
            );
        }
        if section
            .active_agent
            .as_deref()
            .is_some_and(|agent| !agent.is_empty())
            || section
                .steer_sentence_buffer
                .as_deref()
                .is_some_and(|buffer| !buffer.is_empty())
        {
            self.push_line(
                "restore-skipped",
                &format!(
                    "agent identity ({} active, steer buffer {} byte(s)) has no raw-mode pane",
                    section.active_agent.as_deref().unwrap_or("none"),
                    section
                        .steer_sentence_buffer
                        .as_deref()
                        .map(str::len)
                        .unwrap_or(0)
                ),
            );
        }
        if section.tokens_in != 0
            || section.tokens_out != 0
            || section.orchestrator_context_tokens != 0
        {
            self.push_line(
                "restore-skipped",
                &format!(
                    "token witness (in {}, out {}, context {}) is a counter, not a stream line",
                    section.tokens_in, section.tokens_out, section.orchestrator_context_tokens
                ),
            );
        }
        if section.abort_armed {
            self.push_line(
                "restore-skipped",
                "confirm-abort was armed at capture instant; raw mode has no armed window to restore, \
                 and re-arming an abort is never automatic",
            );
        }
    }

    /// The report goes to the same stream, as labelled lines, plus a
    /// `tracing::warn!` — loud, never swallowed. Raw mode has no status line, so
    /// the header line stands in for it.
    fn announce_restore(&mut self, report: &crate::session::format::RepairReport) {
        self.push_line(
            "restore",
            &format!(
                "session restored: {} pane line(s), {} worker(s), {} unrecoverable worker(s)",
                report.ui_lines_restored,
                report.workers_restored,
                report.workers_unrecoverable.len()
            ),
        );
        for notice in &report.notices {
            self.push_line(
                "restore-lost",
                &format!("{:?}: {}: {}", notice.kind, notice.subject, notice.detail),
            );
        }
        for warning in &report.warnings {
            self.push_line("restore-warning", warning);
        }
        tracing::warn!(
            ui_lines_restored = report.ui_lines_restored,
            workers_restored = report.workers_restored,
            notices = report.notices.len(),
            warnings = report.warnings.len(),
            "session restore: repair report surfaced to raw mode"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_raw_renderer_push_line_buffers_labelled_lines() {
        let mut r = RawRenderer::new();
        r.push_line("assistant", "I am on it");
        r.push_line("tool-result", "content");
        let buffered = String::from_utf8(r.buffer.clone()).expect("buffer is valid utf-8");
        assert_eq!(buffered, "[assistant] I am on it\n[tool-result] content\n");
    }

    /// Byte-pin: the verdict word comes from `crate::markers::MARKER_FAILED`,
    /// and every rendered byte is fixed — including the two spaces that
    /// column-align `FAILED` with `STARTED →` / `DONE`, the `(no task id)`
    /// placeholder and the empty-reason filter.
    #[test]
    fn test_delegation_failed_line_keeps_its_exact_bytes() {
        assert_eq!(
            delegation_failed_line(Agent::Coder, Some("t-059"), Some("boom")),
            "FAILED  coder on t-059: boom"
        );
        assert_eq!(
            delegation_failed_line(Agent::Coder, None, None),
            "FAILED  coder on (no task id)"
        );
        assert_eq!(
            delegation_failed_line(Agent::Coder, Some("t-1"), Some("")),
            "FAILED  coder on t-1"
        );
        // The verdict word is the owner's constant, not a local spelling.
        assert_eq!(
            delegation_failed_line(Agent::Coder, Some("t-9"), None),
            format!("{MARKER_FAILED}  coder on t-9")
        );
        // It reaches the wire as one labelled line.
        let mut r = RawRenderer::new();
        r.push_line(
            "delegation",
            &delegation_failed_line(Agent::Coder, Some("t-059"), Some("boom")),
        );
        let buffered = String::from_utf8(r.buffer.clone()).expect("buffer is valid utf-8");
        assert_eq!(buffered, "[delegation] FAILED  coder on t-059: boom\n");
    }

    #[test]
    fn test_raw_renderer_push_line_splits_long_text() {
        let mut r = RawRenderer::new();
        r.push_line("status", &"a".repeat(600));
        let buffered = String::from_utf8(r.buffer.clone()).expect("buffer is valid utf-8");
        let lines: Vec<&str> = buffered.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|l| l.starts_with("[status] ")));
    }

    // ---- rehydration hooks ----

    /// Raw mode owns no pane model, so capture must not claim one: it returns the
    /// empty (but loadable) section.
    #[test]
    fn test_raw_capture_ui_claims_only_what_it_holds() {
        let mut r = RawRenderer::new();
        r.push_line("assistant", "streamed already");
        let section = r.capture_ui();
        assert!(section.chat_messages.is_empty());
        assert!(section.status_line.is_empty());
        assert!(section.subagent_panels.is_empty());
        assert!(section.journal.is_empty());
        assert_eq!(
            section.composer,
            crate::session::format::ComposerState::default()
        );
        assert_eq!(section.view, crate::session::format::ViewState::default());
        assert_eq!(
            section.budgets,
            crate::session::format::BudgetWitness::default()
        );
        // Every group added for pane coverage is unclaimed too: raw mode holds
        // none of it (no plan box, no streaming region, no agent identity, no
        // counters, no armed abort window).
        assert_eq!(section.plan_content, "");
        assert!(!section.view.show_plan_panel);
        assert!(!section.had_active_plan);
        assert_eq!(section.active_plan_task, None);
        assert_eq!(section.plan_start, None);
        assert_eq!(section.plan_completed, None);
        assert_eq!(section.current_thought, "");
        assert_eq!(section.current_content, "");
        assert_eq!(section.active_agent, None);
        assert_eq!(section.steer_sentence_buffer, None);
        assert_eq!(
            (
                section.tokens_in,
                section.tokens_out,
                section.orchestrator_context_tokens
            ),
            (0, 0, 0)
        );
        assert!(!section.abort_armed);
        assert_eq!(section.abort_armed_at, None);
        // An honest empty capture is still a loadable `ui.json` member.
        assert!(section.structural_violations().is_empty());
    }

    /// Restore replays the stored pane text into the pending stream without
    /// reordering, splitting a character, or touching the abort/exit state.
    #[test]
    fn test_raw_restore_panes_replays_without_corrupting_the_stream() {
        let mut r = RawRenderer::new();
        r.push_line("status", "pre-existing");
        let state_before = r.input_state;
        let section = crate::session::format::UiSection {
            chat_messages: vec![
                "User: héllo wörld 🎉".to_string(),
                String::new(),
                "Marmennill: done".to_string(),
            ],
            subagent_panels: vec![crate::session::format::SubagentPanel {
                name: "coder".to_string(),
                task_id: Some("t-011".to_string()),
                prompt: String::new(),
                logs: vec!["one log line".to_string()],
                thinking: String::new(),
                content: String::new(),
                is_active: false,
                context_tokens: 0,
                worked_duration_secs: 0.0,
                started_at: None,
                last_activity_at: None,
                turn_thinking: None,
                is_thinking: None,
            }],
            ..Default::default()
        };

        r.restore_panes(&section);

        // Every byte is still valid UTF-8: nothing was split mid-character.
        let buffered = String::from_utf8(r.buffer.clone()).expect("buffer is valid utf-8");
        assert!(
            buffered.starts_with("[status] pre-existing\n"),
            "the pending stream keeps its order: {buffered:?}"
        );
        assert!(
            buffered.contains("[restore] raw mode has no panes: replaying 3 stored pane line(s)")
        );
        assert!(buffered.contains("[restored] User: héllo wörld 🎉\n"));
        // The empty pane line survives as its own line.
        assert!(buffered.contains("[restored]\n"));
        assert!(buffered.contains("[restored] Marmennill: done\n"));
        assert!(
            buffered
                .contains("[restore-skipped] subagent coder (1 log line(s)) has no raw-mode pane")
        );
        // The stream contract and the abort surface are untouched.
        assert!(!buffered.contains("[done]"));
        assert_eq!(r.input_state.aborted, state_before.aborted);
        assert_eq!(r.input_state.user_exit, state_before.user_exit);
    }

    /// A long multi-byte pane line is chunked on character boundaries, so a
    /// replayed line never emits a partial character.
    #[test]
    fn test_raw_restore_replay_splits_long_lines_on_char_boundaries() {
        let mut r = RawRenderer::new();
        let section = crate::session::format::UiSection {
            chat_messages: vec!["é".repeat(600)],
            ..Default::default()
        };
        r.restore_panes(&section);
        let buffered = String::from_utf8(r.buffer.clone()).expect("buffer is valid utf-8");
        let payload: String = buffered
            .lines()
            .filter(|l| l.starts_with("[restored] "))
            .map(|l| l.trim_start_matches("[restored] ").to_string())
            .collect();
        assert_eq!(payload, "é".repeat(600));
    }

    /// Every pane-coverage group raw mode cannot represent is named on the
    /// stream, so no stored group is dropped silently.
    #[test]
    fn test_raw_restore_names_every_pane_group_it_cannot_represent() {
        let mut r = RawRenderer::new();
        let section = crate::session::format::UiSection {
            plan_content: "# Plan\n- [ ] t-020 close the gap".to_string(),
            had_active_plan: true,
            active_plan_task: Some("t-020".to_string()),
            current_thought: "thinking bytes".to_string(),
            current_content: "answer bytes".to_string(),
            active_agent: Some("coder-t-020".to_string()),
            steer_sentence_buffer: Some("half a steer".to_string()),
            tokens_in: 1234,
            tokens_out: 567,
            orchestrator_context_tokens: 89,
            abort_armed: true,
            view: crate::session::format::ViewState {
                show_plan_panel: true,
                ..Default::default()
            },
            ..Default::default()
        };
        assert!(section.structural_violations().is_empty());

        r.restore_panes(&section);
        let buffered = String::from_utf8(r.buffer.clone()).expect("buffer is valid utf-8");
        assert!(buffered.contains(
            "[restore] raw mode has no panes: replaying 0 stored pane line(s); composer/view/subagents/budgets/plan/streaming/identity/tokens/abort fields are not representable here"
        ));
        assert!(buffered.contains(
            "[restore-skipped] plan pane (32 byte(s) of plan text, panel visible: true, in-flight task: t-020) has no raw-mode pane"
        ));
        assert!(buffered.contains(
            "[restore-skipped] in-flight streaming partials (14 thought byte(s), 12 content byte(s)) have no raw-mode pane"
        ));
        assert!(buffered.contains(
            "[restore-skipped] agent identity (coder-t-020 active, steer buffer 12 byte(s)) has no raw-mode pane"
        ));
        assert!(buffered.contains(
            "[restore-skipped] token witness (in 1234, out 567, context 89) is a counter, not a stream line"
        ));
        assert!(buffered.contains(
            "[restore-skipped] confirm-abort was armed at capture instant; raw mode has no armed window to restore, and re-arming an abort is never automatic"
        ));
        // Nothing was invented: no pane line was replayed that was never stored.
        assert!(!buffered.contains("[restored]"));
        assert!(!buffered.contains("[done]"));
    }

    #[test]
    fn test_raw_announce_restore_writes_every_loss_as_its_own_line() {
        let mut r = RawRenderer::new();
        let report = crate::session::format::RepairReport {
            ui_lines_restored: 7,
            workers_restored: 2,
            workers_unrecoverable: vec!["coder-t-1".to_string()],
            notices: vec![crate::session::format::NonRestorableNotice {
                kind: crate::session::format::NonRestorableKind::JoinHandle,
                subject: "coder".to_string(),
                detail: "no task exists to join or abort".to_string(),
            }],
            warnings: vec!["thinking budget drifted: 8000 -> 4000".to_string()],
            ..Default::default()
        };
        r.announce_restore(&report);
        let buffered = String::from_utf8(r.buffer.clone()).expect("buffer is valid utf-8");
        assert!(buffered.contains(
            "[restore] session restored: 7 pane line(s), 2 worker(s), 1 unrecoverable worker(s)"
        ));
        assert!(
            buffered.contains("[restore-lost] JoinHandle: coder: no task exists to join or abort")
        );
        assert!(buffered.contains("[restore-warning] thinking budget drifted: 8000 -> 4000"));
    }
}
