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
    /// Shared abort / user-exit flags (trait-default abort surface).
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
/// Gate t-059: the verdict word is the marker owner's constant
/// ([`crate::markers::MARKER_FAILED`]) rather than a hand-typed literal, so the
/// vocabulary has one owner. The rendered bytes are unchanged — `FAILED` is
/// followed by exactly two spaces to column-align with `STARTED →` / `DONE` —
/// and that alignment is pinned by `test_delegation_failed_line_keeps_its_exact_bytes`.
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

    /// Gate t-059 byte-pin: the verdict word now comes from
    /// `crate::markers::MARKER_FAILED`, and every rendered byte is unchanged —
    /// including the two spaces that column-align `FAILED` with `STARTED →` /
    /// `DONE`, the `(no task id)` placeholder and the empty-reason filter.
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
}
