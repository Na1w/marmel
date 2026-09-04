//! Headless (raw) streaming mode.
//!
//! This renderer streams agent events directly to stdout in a pipe-friendly
//! way (one logical unit per line), so `marmel --raw "explain main.rs"` can be
//! captured by scripts. It never enters raw mode or the alternate screen, so
//! the terminal is always left in a sane state.

use super::{Event, Renderer, chunk_utf8};
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
    aborted: bool,
}

impl RawRenderer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            aborted: false,
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

    fn poll_input(&mut self) -> Option<String> {
        // In headless mode there is no interactive stdin prompt; the CLI
        // positional prompt (or `None`) already supplied the goal.
        None
    }

    fn read_input(&mut self) -> Option<String> {
        // No interactive input in headless mode.
        None
    }

    fn request_abort(&mut self) {
        self.aborted = true;
    }

    fn aborted(&self) -> bool {
        self.aborted
    }

    fn clear_abort(&mut self) {
        self.aborted = false;
    }

    fn shutdown(&mut self) {
        let _ = self.flush();
    }

    fn rehydrate_messages(&mut self, messages: &[crate::types::Message]) {
        let delegated_call_ids: std::collections::HashSet<&str> = messages
            .iter()
            .filter_map(|m| match m {
                crate::types::Message::Assistant { tool_calls, .. } => Some(tool_calls),
                _ => None,
            })
            .flatten()
            .filter(|call| call.function.name == "delegate_task")
            .map(|call| call.id.as_str())
            .collect();

        for msg in messages.iter().skip(1) {
            match msg {
                crate::types::Message::User { content } => {
                    if !content.starts_with("(SYSTEM NOTICE:") && !content.starts_with("(SYSTEM:") {
                        self.push_line("user", content);
                    }
                }
                crate::types::Message::Assistant {
                    content,
                    tool_calls,
                    ..
                } => {
                    for call in tool_calls {
                        self.push_line(
                            "tool",
                            &format!("{}({})", call.function.name, call.function.arguments),
                        );
                    }
                    if let Some(c) = content
                        && !c.trim().is_empty()
                    {
                        self.push_line("assistant", c);
                    }
                }
                crate::types::Message::Tool {
                    tool_call_id,
                    content,
                } => {
                    if delegated_call_ids.contains(tool_call_id.as_str()) {
                        let summary = if let Some(first_line) = content.lines().next() {
                            if first_line.starts_with("MISSION COMPLETE") {
                                first_line.to_string()
                            } else {
                                "Task completed".to_string()
                            }
                        } else {
                            "Task completed".to_string()
                        };
                        self.push_line("tool-result", &summary);
                    } else {
                        self.push_line("tool-result", content);
                    }
                }
                _ => {}
            }
        }
        let _ = self.flush();
    }
}
