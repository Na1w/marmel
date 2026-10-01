//! Headless (raw) streaming mode.
//!
//! This renderer streams agent events directly to stdout in a pipe-friendly
//! way (one logical unit per line), so `marmel --raw "explain main.rs"` can be
//! captured by scripts. It never enters raw mode or the alternate screen, so
//! the terminal is always left in a sane state.

use super::{Event, InputState, Renderer, chunk_utf8};
use crate::config::Config;
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
                crate::orchestrator::DelegationEvent::Failed { agent, task } => {
                    let t = task.as_deref().unwrap_or("(no task id)");
                    self.push_line("delegation", &format!("FAILED  {agent} on {t}"));
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

    fn rehydrate_ui(&mut self, records: &[crate::ui::UiRecord]) {
        for rec in records {
            match rec {
                crate::ui::UiRecord::User { text } => {
                    self.push_line("user", text);
                }
                crate::ui::UiRecord::Assistant { content, thinking } => {
                    if let Some(r) = thinking
                        && !r.trim().is_empty()
                    {
                        self.push_line("thinking", r);
                    }
                    if let Some(c) = content
                        && !c.trim().is_empty()
                    {
                        self.push_line("assistant", c);
                    }
                }
                crate::ui::UiRecord::SteerResponse { text } => {
                    self.push_line("steer", text);
                }
                crate::ui::UiRecord::ToolCall { display } => {
                    self.push_line("tool", display);
                }
                crate::ui::UiRecord::ToolResult { display } => {
                    self.push_line("tool-result", display);
                }
                crate::ui::UiRecord::TaskCompleted { task_id } => {
                    self.push_line("delegation", &format!("DONE    specialist on {task_id}"));
                }
                crate::ui::UiRecord::TaskFailed { task_id } => {
                    self.push_line("delegation", &format!("FAILED  specialist on {task_id}"));
                }
                crate::ui::UiRecord::Status { text } => {
                    self.push_line("status", text);
                }
            }
        }
        let _ = self.flush();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::UiRecord;

    #[test]
    fn test_raw_renderer_rehydrate_ui() {
        let mut r = RawRenderer::new();
        let records = vec![
            UiRecord::User {
                text: "My task".to_string(),
            },
            UiRecord::Assistant {
                content: Some("I am on it".to_string()),
                thinking: Some("Reasoning here".to_string()),
            },
            UiRecord::SteerResponse {
                text: "Steer text".to_string(),
            },
            UiRecord::ToolCall {
                display: "read_file(foo.txt)".to_string(),
            },
            UiRecord::ToolResult {
                display: "content".to_string(),
            },
            UiRecord::TaskCompleted {
                task_id: "t-1".to_string(),
            },
            UiRecord::TaskFailed {
                task_id: "t-2".to_string(),
            },
            UiRecord::Status {
                text: "Running tests".to_string(),
            },
        ];

        // Calling rehydrate_ui should not panic and should properly format
        r.rehydrate_ui(&records);
    }
}
