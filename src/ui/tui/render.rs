//! Ratatui frame drawing and panel rendering for TuiRenderer.

use super::TuiRenderer;
use super::formatting::*;
use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::io;

impl TuiRenderer {
    /// Ensure the per-message wrapped-line cache is up to date for `width`.
    /// Performs a full recompute when the width or
    /// `show_thought` changes, an incremental append when new messages are
    /// added, and a truncate when messages are removed.
    pub(crate) fn ensure_message_cache(&self, width: usize) {
        self.ensure_rendered_message_cache();
        let cached_w = self.cached_chat_width.get();
        let cached_st = self.cached_show_thought.get();
        let mut cached_lines = self.cached_message_lines.borrow_mut();
        let rendered = self.cached_rendered_messages.borrow();

        if cached_w != width
            || cached_st != self.show_thought
            || cached_lines.len() != rendered.len()
        {
            if cached_w != width || cached_st != self.show_thought {
                self.cached_chat_width.set(width);
                self.cached_show_thought.set(self.show_thought);
                cached_lines.clear();
                let mut total = 0;
                for msg_lines in rendered.iter() {
                    let lines = count_wrapped_rendered_lines(msg_lines, width);
                    cached_lines.push(lines);
                    total += lines;
                }
                self.cached_total_message_lines.set(total);
            } else if cached_lines.len() < rendered.len() {
                let start = cached_lines.len();
                let mut total = self.cached_total_message_lines.get();
                for msg_lines in &rendered[start..] {
                    let lines = count_wrapped_rendered_lines(msg_lines, width);
                    cached_lines.push(lines);
                    total += lines;
                }
                self.cached_total_message_lines.set(total);
            } else if cached_lines.len() > rendered.len() {
                cached_lines.truncate(rendered.len());
                let total: usize = cached_lines.iter().sum();
                self.cached_total_message_lines.set(total);
            }
        }
    }

    /// Recompute only the last message's line count (used when a `Marmennill:`
    /// message is appended to during steer streaming) and update the total.
    pub(crate) fn invalidate_last_message_cache(&self) {
        let last_idx = match self.messages.len().checked_sub(1) {
            Some(i) => i,
            None => return,
        };

        let new_rendered = self.render_single_message(&self.messages[last_idx]);
        let width = self.cached_chat_width.get();
        let new_lines = count_wrapped_rendered_lines(&new_rendered, width);

        let mut cached_rendered = self.cached_rendered_messages.borrow_mut();
        if cached_rendered.len() == self.messages.len() {
            cached_rendered[last_idx] = new_rendered;
        } else if cached_rendered.len() == last_idx {
            cached_rendered.push(new_rendered);
        }
        drop(cached_rendered);

        let mut cached_lines = self.cached_message_lines.borrow_mut();
        if cached_lines.len() == self.messages.len() {
            let old = cached_lines[last_idx];
            cached_lines[last_idx] = new_lines;
            let total = self.cached_total_message_lines.get().saturating_sub(old) + new_lines;
            self.cached_total_message_lines.set(total);
        } else if cached_lines.len() == last_idx {
            cached_lines.push(new_lines);
            let total = self.cached_total_message_lines.get() + new_lines;
            self.cached_total_message_lines.set(total);
        }
    }

    /// Render a single message into styled lines.
    pub(crate) fn render_single_message(&self, msg: &str) -> Vec<Line<'static>> {
        render_message_lines(msg, self.show_thought)
    }

    /// Keep the rendered-message cache up to date for the current `messages` and `show_thought`.
    pub(crate) fn ensure_rendered_message_cache(&self) {
        let cached_st = self.cached_rendered_show_thought.get();
        let mut cached = self.cached_rendered_messages.borrow_mut();
        if cached_st != self.show_thought {
            self.cached_rendered_show_thought.set(self.show_thought);
            cached.clear();
        }
        if cached.len() < self.messages.len() {
            let start = cached.len();
            for msg in &self.messages[start..] {
                cached.push(self.render_single_message(msg));
            }
        } else if cached.len() > self.messages.len() {
            cached.truncate(self.messages.len());
        }
    }

    /// Render streaming thought lines.
    pub(crate) fn render_streaming_thought(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let think_style = Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::ITALIC);
        for line in self.current_thought.lines() {
            let line = line.replace('\t', "    ");
            let trimmed = line.trim();
            if trimmed == "<think>"
                || trimmed == "</think>"
                || trimmed == "<thought>"
                || trimmed == "</thought>"
                || trimmed == "<think></think>"
                || trimmed == "<thought></thought>"
            {
                continue;
            }
            let cleaned = format_terminal_math(&strip_think_tags(&line));
            if cleaned.trim().is_empty() && !line.trim().is_empty() {
                continue;
            }
            lines.push(Line::from(Span::styled(cleaned, think_style)));
        }
        lines
    }

    /// Render streaming content lines.
    pub(crate) fn render_streaming_content(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        if self.current_content.contains("```") {
            let cleaned = strip_think_tags(&self.current_content);
            lines.extend(render_markdown_lines(&cleaned));
        } else {
            for raw_line in self.current_content.lines() {
                let line = raw_line.replace('\t', "    ");
                let trimmed = line.trim();
                if trimmed == "<think>"
                    || trimmed == "</think>"
                    || trimmed == "<thought>"
                    || trimmed == "</thought>"
                    || trimmed == "<think></think>"
                    || trimmed == "<thought></thought>"
                {
                    continue;
                }
                let cleaned = format_terminal_math(&strip_think_tags(&line));
                if cleaned.trim().is_empty() && !line.trim().is_empty() {
                    continue;
                }
                lines.push(Line::from(Span::styled(
                    cleaned,
                    Style::default().fg(Color::White),
                )));
            }
        }
        lines
    }

    /// Render streaming steer sentence lines.
    pub(crate) fn render_streaming_steer(&self) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let prefix = if self
            .messages
            .last()
            .is_some_and(|m| m.starts_with("Marmennill: "))
        {
            ""
        } else {
            "Marmennill: "
        };
        let formatted = format!("{prefix}{}", self.steer_sentence_buffer);
        let steer_style = Style::default()
            .fg(Color::LightYellow)
            .add_modifier(Modifier::BOLD);
        for line in formatted.lines() {
            let line = line.replace('\t', "    ");
            lines.push(Line::from(Span::styled(line, steer_style)));
        }
        lines
    }

    /// Estimate the total wrapped line count of the chat transcript at `width`,
    /// including streaming buffers.
    pub(crate) fn estimated_chat_lines(&self, width: usize) -> usize {
        self.ensure_message_cache(width);
        let mut n = self.cached_total_message_lines.get();
        if self.show_thought && !self.current_thought.is_empty() {
            let think_lines = self.render_streaming_thought();
            n += count_wrapped_rendered_lines(&think_lines, width);
        }
        if !self.current_content.is_empty() {
            let content_lines = self.render_streaming_content();
            n += count_wrapped_rendered_lines(&content_lines, width);
        }
        if !self.steer_sentence_buffer.is_empty() {
            let steer_lines = self.render_streaming_steer();
            n += count_wrapped_rendered_lines(&steer_lines, width);
        }
        n
    }

    /// Estimate the total wrapped line count of the selected subagent's
    /// details pane at `width`.
    pub(crate) fn estimated_subagent_lines(&self, width: usize) -> usize {
        if let Some(sa) = self.subagents.get(self.selected_subagent_idx) {
            let mut n = 1; // "=== Details for ... ==="
            if !sa.thinking.is_empty() && self.show_thought {
                n += 2; // "[Thinking]", " thinking"
                for line in sa.thinking.lines() {
                    n += if line.is_empty() {
                        1
                    } else {
                        wrapped_lines(line, width)
                    };
                }
                n += 1; // " response"
            }
            if !sa.content.is_empty() {
                n += 1; // "[Output]"
                for line in sa.content.lines() {
                    n += if line.is_empty() {
                        1
                    } else {
                        wrapped_lines(line, width)
                    };
                }
            }
            if !sa.logs.is_empty() {
                n += 1; // "[Logs]"
                for log in &sa.logs {
                    let log_line = format!("- {log}");
                    n += if log_line.is_empty() {
                        1
                    } else {
                        wrapped_lines(&log_line, width)
                    };
                }
            }
            n
        } else {
            1
        }
    }

    pub(crate) fn draw(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        // Refresh the plan content from disk (live check-off updates), cached to avoid synchronous disk I/O on every frame.
        let plan_path = std::path::Path::new(crate::manager::phase::MARMEL_DIR)
            .join(crate::manager::phase::PLAN_FILE);
        let archive_path = std::path::Path::new(crate::manager::phase::MARMEL_DIR)
            .join("execution_plan_archive.md");

        if self.last_plan_check.elapsed() >= std::time::Duration::from_millis(250)
            || self.plan_content.is_empty()
        {
            self.last_plan_check = std::time::Instant::now();
            let current_plan_mtime = std::fs::metadata(&plan_path)
                .ok()
                .and_then(|m| m.modified().ok());
            let current_archive_mtime = std::fs::metadata(&archive_path)
                .ok()
                .and_then(|m| m.modified().ok());

            if current_plan_mtime != self.plan_mtime
                || current_archive_mtime != self.archive_mtime
                || self.plan_content.is_empty()
            {
                self.plan_mtime = current_plan_mtime;
                self.archive_mtime = current_archive_mtime;

                let (plan, is_archived, has_active_plan) = if current_plan_mtime.is_some() {
                    let content = std::fs::read_to_string(&plan_path)
                        .unwrap_or_else(|_| "Error reading execution plan.".to_string());
                    let active = !content.trim().is_empty();
                    (content, false, active)
                } else if current_archive_mtime.is_some() {
                    (
                        format!(
                            "[Plan completed & archived to .marmel/archive/]\n\n{}",
                            std::fs::read_to_string(&archive_path).unwrap_or_default()
                        ),
                        true,
                        false,
                    )
                } else {
                    (
                        "No active execution plan on disk.".to_string(),
                        false,
                        false,
                    )
                };
                self.plan_content = plan;
                self.plan_is_archived = is_archived;

                // Auto-open plan panel when a plan appears; keep it visible so user can always see plan progress.
                if (has_active_plan || is_archived) && !self.had_active_plan {
                    self.show_plan_panel = true;
                    self.plan_scroll = 0;
                    self.plan_auto_scroll = true;
                }
                self.had_active_plan = has_active_plan || is_archived;
            }
        }

        if self.chat_auto_scroll {
            let w = self.chat_width.get();
            let n = self.estimated_chat_lines(w);
            let h = self.chat_height.get();
            self.chat_scroll = n.saturating_sub(h) as u16;
        }

        if self.subagent_autoscroll {
            let w = self.subagent_width.get();
            let n = self.estimated_subagent_lines(w);
            let h = self.subagent_height.get();
            self.subagent_scroll = n.saturating_sub(h) as u16;
        } else {
            let w = self.subagent_width.get();
            let n = self.estimated_subagent_lines(w);
            let h = self.subagent_height.get();
            let max_scroll = n.saturating_sub(h) as u16;
            if self.subagent_scroll > max_scroll {
                self.subagent_scroll = max_scroll;
            }
        }

        let mut chat_area = Rect::default();
        let mut plan_area = Rect::default();
        let mut subagent_area = Rect::default();
        let mut input_area = Rect::default();

        // The frame body is the rebuilt layout: `layout::compute_layout` replaces
        // the legacy `[Min(3), Length(1), Length(3)]` root split, the
        // `Percentage(60)/Percentage(40)` main split and the
        // `Percentage(50)/Percentage(50)` right-column pair (spec §2.1, §2.3),
        // and `draw_new_ui` renders every pane component of §3 into those rects.
        terminal.draw(|frame| {
            let rects = self.draw_new_ui(frame);
            chat_area = rects.transcript.unwrap_or_default();
            plan_area = rects.plan.unwrap_or_default();
            subagent_area = rects.workers.unwrap_or_default();
            input_area = rects.input.unwrap_or_default();
        })?;

        self.chat_area = chat_area;
        self.plan_area = plan_area;
        self.subagent_area = subagent_area;
        self.input_area = input_area;
        Ok(())
    }
}
