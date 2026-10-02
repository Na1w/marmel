//! Ratatui frame drawing and panel rendering for TuiRenderer.

use super::formatting::*;
use super::{FocusedPanel, TuiRenderer};
use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState, Wrap,
};
use std::io;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

impl TuiRenderer {
    /// Ensure the per-message wrapped-line cache is up to date for `width`
    /// (reference §11.3). Performs a full recompute when the width or
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
                // Full recompute.
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
                // Incremental append (new messages added).
                let start = cached_lines.len();
                let mut total = self.cached_total_message_lines.get();
                for msg_lines in &rendered[start..] {
                    let lines = count_wrapped_rendered_lines(msg_lines, width);
                    cached_lines.push(lines);
                    total += lines;
                }
                self.cached_total_message_lines.set(total);
            } else if cached_lines.len() > rendered.len() {
                // Truncate (messages removed).
                cached_lines.truncate(rendered.len());
                let total: usize = cached_lines.iter().sum();
                self.cached_total_message_lines.set(total);
            }
        }
    }

    /// Recompute only the last message's line count (used when a `Marmennill:`
    /// message is appended to during steer streaming) and update the total
    /// (reference §11.4).
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

    /// Ensure the pre-rendered message cache is up to date for current `messages` and `show_thought`.
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

    /// Estimate the total wrapped line count of the chat transcript at `width`
    /// (reference §11.5), including streaming buffers.
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
    /// details pane at `width` (reference §11.6).
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

    /// Draw one complete frame using the current buffer state.
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

        // Auto-scroll targets (reference §11.5 / §11.6).
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

        terminal.draw(|frame| {
            let size = frame.area();

            // 3.1 Vertical split: Main (Min 3) + Status (Length 1) + Input (Length 3).
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Min(3),
                    Constraint::Length(1),
                    Constraint::Length(3),
                ])
                .split(size);
            let main_area = chunks[0];
            let status_area = chunks[1];
            let input_area = chunks[2];

            // 3.2 Horizontal split: Chat (60%) + right panel (40%).
            let has_plan = !self.plan_content.trim().is_empty() && self.show_plan_panel;
            let has_subagents = self.show_subagent_panel && !self.subagents.is_empty();

            let (chat_rect, right_area) = if has_plan || has_subagents {
                let main_chunks = Layout::default()
                    .direction(Direction::Horizontal)
                    .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
                    .split(main_area);
                (main_chunks[0], Some(main_chunks[1]))
            } else {
                (main_area, None)
            };

            // 3.3 Right panel split: Plan (top 50%) + Subagents (bottom 50%).
            let mut plan_rect = None;
            let mut subagent_rect = None;
            if let Some(r_area) = right_area {
                if has_plan && has_subagents {
                    let right_chunks = Layout::default()
                        .direction(Direction::Vertical)
                        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
                        .split(r_area);
                    plan_rect = Some(right_chunks[0]);
                    subagent_rect = Some(right_chunks[1]);
                } else if has_plan {
                    plan_rect = Some(r_area);
                } else if has_subagents {
                    subagent_rect = Some(r_area);
                }
            }

            chat_area = chat_rect;
            if let Some(p) = plan_rect {
                plan_area = p;
            }
            if let Some(s) = subagent_rect {
                subagent_area = s;
            }

            self.render_chat(frame, chat_area);
            if let Some(p) = plan_rect {
                let plan = self.plan_content.clone();
                let is_archived = self.plan_is_archived;
                self.render_plan(frame, p, &plan, is_archived);
            }
            if let Some(s) = subagent_rect {
                self.render_subagents(frame, s);
            }
            self.render_status(frame, status_area);
            self.render_input(frame, input_area);
        })?;

        self.chat_area = chat_area;
        self.plan_area = plan_area;
        self.subagent_area = subagent_area;
        Ok(())
    }

    pub(crate) fn render_chat(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let chat_border_color = if self.focused_panel == FocusedPanel::Chat {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let chat_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(chat_border_color))
            .title(" Chat & Logs (Focus: Tab) ");

        let chat_w = area.width.saturating_sub(2) as usize;
        let chat_h = area.height.saturating_sub(2) as usize;
        self.chat_width.set(chat_w);
        self.chat_height.set(chat_h);

        let mut chat_lines = Vec::new();
        self.ensure_rendered_message_cache();
        {
            let cached = self.cached_rendered_messages.borrow();
            for msg_lines in &*cached {
                chat_lines.extend(msg_lines.iter().cloned());
            }
        }

        // Streaming thoughts (reference §4.4.1).
        if self.show_thought && !self.current_thought.is_empty() {
            chat_lines.extend(self.render_streaming_thought());
        }

        // Streaming content (Orchestrator output - WHITE or syntax highlighted).
        if !self.current_content.is_empty() {
            chat_lines.extend(self.render_streaming_content());
        }

        // Streaming steer sentence (Steer Arbitrator output - YELLOW BOLD).
        if !self.steer_sentence_buffer.is_empty() {
            chat_lines.extend(self.render_streaming_steer());
        }

        let total_chat_lines = self.estimated_chat_lines(chat_w);

        let max_scroll = total_chat_lines.saturating_sub(chat_h);
        if self.chat_auto_scroll || (self.chat_scroll as usize) > max_scroll {
            self.chat_scroll = max_scroll as u16;
        }

        let chat_paragraph = Paragraph::new(chat_lines)
            .block(chat_block)
            .wrap(Wrap { trim: false })
            .scroll((self.chat_scroll, 0));
        frame.render_widget(chat_paragraph, area);

        // F4: vertical scrollbar on the chat pane.
        if max_scroll > 0 {
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(self.chat_scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                inner,
                &mut scrollbar_state,
            );
        }
    }

    pub(crate) fn render_plan(
        &mut self,
        frame: &mut ratatui::Frame,
        area: Rect,
        plan: &str,
        _is_archived: bool,
    ) {
        let plan_border_color = if self.focused_panel == FocusedPanel::Plan {
            Color::Cyan
        } else {
            Color::DarkGray
        };
        let plan_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(plan_border_color))
            .title(" Execution Plan ");

        // Gather ALL active tasks across active subagents and active_plan_task
        let mut active_tasks: Vec<String> = Vec::new();
        for s in &self.subagents {
            if s.is_active {
                let tid_opt = s.task_id.as_deref().or_else(|| {
                    if let Some(pos) = s.name.find("-t-") {
                        Some(&s.name[pos + 1..])
                    } else {
                        None
                    }
                });
                if let Some(tid) = tid_opt {
                    let clean = clean_task_id(tid);
                    if !clean.is_empty()
                        && !active_tasks.iter().any(|t| t.eq_ignore_ascii_case(clean))
                    {
                        active_tasks.push(clean.to_string());
                    }
                }
            }
        }
        if let Some(tid) = &self.active_plan_task {
            let clean = clean_task_id(tid);
            if !clean.is_empty() && !active_tasks.iter().any(|t| t.eq_ignore_ascii_case(clean)) {
                active_tasks.push(clean.to_string());
            }
        }

        let target_task = self.active_plan_task.as_deref().or_else(|| {
            self.subagents
                .iter()
                .find(|s| s.is_active && s.task_id.is_some())
                .and_then(|s| s.task_id.as_deref())
        });

        let mut plan_lines = Vec::new();
        for raw_line in plan.lines() {
            let line = raw_line.replace('\t', "    ");
            let is_active_task = active_tasks
                .iter()
                .any(|tid| line_matches_task_id(&line, tid));

            let style = if line.starts_with("# ") || line.starts_with("## ") {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else if is_active_task && (line.contains("[ ]") || line.contains("( )")) {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else if line.contains("[x]") || line.contains("[X]") {
                Style::default().fg(Color::Green)
            } else if line.contains("[ ]") {
                Style::default().fg(Color::LightRed)
            } else {
                Style::default()
            };
            plan_lines.push(Line::styled(line, style));
        }

        let plan_w = area.width.saturating_sub(2) as usize;
        let plan_h = area.height.saturating_sub(2) as usize;
        self.plan_width.set(plan_w);
        self.plan_height.set(plan_h);

        let total_plan_lines = wrapped_lines(plan, plan_w.max(1));
        self.plan_max_scroll = total_plan_lines.saturating_sub(plan_h) as u16;
        let scroll_y = if self.plan_auto_scroll {
            let target_scroll = if let Some(task_id) = target_task
                && let Some(task_visual) = visual_line_offset_of_task(plan, task_id, plan_w.max(1))
            {
                // Position the active started task comfortably in view (leaving 1 line of context above if possible)
                let desired = task_visual.saturating_sub(1);
                (desired as u16).min(self.plan_max_scroll)
            } else if let Some(first_pending_visual) =
                visual_line_offset_of_first_pending(plan, plan_w.max(1))
            {
                // Position the first pending task comfortably in view (leaving 1 line of context above if possible)
                let desired = first_pending_visual.saturating_sub(1);
                (desired as u16).min(self.plan_max_scroll)
            } else {
                // If all tasks are completed or no pending tasks, scroll to bottom to show completed progress
                self.plan_max_scroll
            };
            self.plan_scroll = target_scroll;
            target_scroll
        } else {
            self.plan_scroll.min(self.plan_max_scroll)
        };

        let paragraph = Paragraph::new(plan_lines)
            .block(plan_block)
            .wrap(Wrap { trim: false })
            .scroll((scroll_y, 0));
        frame.render_widget(paragraph, area);

        // F4: vertical scrollbar on the plan pane.
        if self.plan_max_scroll > 0 {
            let inner = Rect {
                x: area.x + 1,
                y: area.y + 1,
                width: area.width.saturating_sub(2),
                height: area.height.saturating_sub(2),
            };
            let mut scrollbar_state = ScrollbarState::new(self.plan_max_scroll as usize)
                .position(self.plan_scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                inner,
                &mut scrollbar_state,
            );
        }
    }

    pub(crate) fn render_subagents(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        let subagent_border_color = if self.focused_panel == FocusedPanel::Subagents {
            Color::Cyan
        } else {
            Color::DarkGray
        };

        let total = self.subagents.len();
        let title = if total == 0 {
            " Specialist Subagents (Ctrl-A | ◄/► Agent | ▲/▼ Scroll) ".to_string()
        } else {
            let current = (self.selected_subagent_idx + 1).min(total);
            format!(" Specialist Subagents [{current}/{total}] (Ctrl-A | ◄/► Agent | ▲/▼ Scroll) ")
        };

        let subagent_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(subagent_border_color))
            .title(title);

        let inner_area = subagent_block.inner(area);
        let list_len = (self.subagents.len() as u16)
            .clamp(1, 3)
            .min(inner_area.height.saturating_sub(3).max(1));
        let subagent_chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(list_len), Constraint::Min(3)])
            .split(inner_area);

        // Subagent list.
        let mut list_lines = Vec::new();
        for (i, sa) in self.subagents.iter().enumerate() {
            let prefix = if i == self.selected_subagent_idx {
                "> "
            } else {
                "  "
            };
            let name_style = if sa.is_active {
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let ctx_tokens = if sa.is_active {
                crate::orchestrator::get_active_worker_tokens(&sa.name).unwrap_or(sa.context_tokens)
            } else {
                sa.context_tokens
            };
            let ctx_info = if ctx_tokens > 0 {
                format!(", {} ctx", Self::format_count(ctx_tokens))
            } else {
                String::new()
            };
            let total_secs = if sa.is_active {
                (sa.worked_duration + sa.started_at.map(|st| st.elapsed()).unwrap_or_default())
                    .as_secs()
            } else {
                sa.worked_duration.as_secs()
            };
            let elapsed_str = if total_secs > 0 {
                if total_secs >= 60 {
                    format!(", {}m {}s", total_secs / 60, total_secs % 60)
                } else {
                    format!(", {}s", total_secs)
                }
            } else {
                String::new()
            };
            let status = if sa.is_active {
                Span::styled(
                    format!(" (Active{ctx_info}{elapsed_str})"),
                    Style::default().fg(Color::LightGreen),
                )
            } else {
                Span::styled(
                    format!(" (Idle{ctx_info}{elapsed_str})"),
                    Style::default().fg(Color::DarkGray),
                )
            };
            list_lines.push(Line::from(vec![
                Span::raw(prefix),
                Span::styled(sa.name.clone(), name_style),
                status,
            ]));
        }
        if list_lines.is_empty() {
            list_lines.push(Line::raw("No subagents."));
        }

        // Auto-adjust list scroll to keep the selected subagent visible
        // (reference §6.3).
        let list_height = subagent_chunks[0].height as usize;
        let mut list_scroll = self.subagent_list_scroll as usize;
        if list_height > 0 {
            if self.selected_subagent_idx < list_scroll {
                list_scroll = self.selected_subagent_idx;
            } else if self.selected_subagent_idx >= list_scroll + list_height {
                list_scroll = self.selected_subagent_idx + 1 - list_height;
            }
            let max_scroll = self.subagents.len().saturating_sub(list_height);
            if list_scroll > max_scroll {
                list_scroll = max_scroll;
            }
        } else {
            list_scroll = 0;
        }
        self.subagent_list_scroll = list_scroll as u16;

        let list_paragraph = Paragraph::new(list_lines)
            .wrap(Wrap { trim: false })
            .scroll((list_scroll as u16, 0));
        frame.render_widget(list_paragraph, subagent_chunks[0]);

        // Separator.
        let separator = Block::default()
            .borders(Borders::TOP)
            .border_style(Style::default().fg(Color::DarkGray));

        // Subagent details and bottom status bar.
        let sa_opt = self.subagents.get(self.selected_subagent_idx);
        let show_status_bar = if let Some(sa) = sa_opt {
            sa.is_active || !sa.thinking.is_empty()
        } else {
            false
        };

        let (details_area, status_area_opt) = if show_status_bar && subagent_chunks[1].height >= 4 {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // Top separator border
                    Constraint::Min(2),    // Scrollable details area
                    Constraint::Length(1), // Bottom subagent status bar
                ])
                .split(subagent_chunks[1]);
            frame.render_widget(separator, chunks[0]);
            (chunks[1], Some(chunks[2]))
        } else {
            let chunks = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(1), // Top separator border
                    Constraint::Min(2),    // Scrollable details area
                ])
                .split(subagent_chunks[1]);
            frame.render_widget(separator, chunks[0]);
            (chunks[1], None)
        };

        let mut detail_lines = Vec::new();
        if let Some(sa) = sa_opt {
            detail_lines.push(Line::styled(
                format!("=== Details for {} ===", sa.name),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ));

            if !sa.thinking.is_empty() && self.show_thought {
                detail_lines.push(Line::styled(
                    "[Thinking]",
                    Style::default()
                        .fg(Color::Magenta)
                        .add_modifier(Modifier::BOLD),
                ));
                let think_style = Style::default()
                    .fg(Color::DarkGray)
                    .add_modifier(Modifier::ITALIC);
                detail_lines.push(Line::styled(" thinking", think_style));
                for line in sa.thinking.lines() {
                    let line = format_terminal_math(line);
                    detail_lines.push(Line::styled(line, think_style));
                }
                detail_lines.push(Line::styled(" response", think_style));
            }

            if !sa.content.is_empty() {
                detail_lines.push(Line::styled(
                    "[Output]",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                if sa.content.contains("```") {
                    detail_lines.extend(render_markdown_lines(&sa.content));
                } else {
                    for line in sa.content.lines() {
                        let line = format_terminal_math(line);
                        detail_lines.push(Line::raw(line));
                    }
                }
            }

            if !sa.logs.is_empty() {
                detail_lines.push(Line::styled(
                    "[Logs]",
                    Style::default()
                        .fg(Color::Green)
                        .add_modifier(Modifier::BOLD),
                ));
                for log in &sa.logs {
                    if log.contains('\x1b') {
                        let parsed = parse_ansi_lines(log);
                        for line in parsed {
                            let mut spans = vec![Span::raw("- ")];
                            spans.extend(line.spans);
                            detail_lines.push(Line::from(spans));
                        }
                    } else {
                        detail_lines.push(Line::raw(format!("- {log}")));
                    }
                }
            }
        } else {
            detail_lines.push(Line::raw("No subagents active or selected."));
        }

        let details_w = details_area.width.saturating_sub(1) as usize;
        let details_h = details_area.height as usize;
        self.subagent_width.set(details_w);
        self.subagent_height.set(details_h);

        let total_detail_lines = count_wrapped_rendered_lines(&detail_lines, details_w);

        let max_scroll = total_detail_lines.saturating_sub(details_h);
        if self.subagent_autoscroll || (self.subagent_scroll as usize) > max_scroll {
            self.subagent_scroll = max_scroll as u16;
        }

        let detail_paragraph = Paragraph::new(detail_lines)
            .wrap(Wrap { trim: false })
            .scroll((self.subagent_scroll, 0));
        frame.render_widget(detail_paragraph, details_area);

        // Subagent bottom status bar (rendered like main UI status bar).
        if let (Some(status_area), Some(sa)) = (status_area_opt, sa_opt) {
            let frames = ["-", "\\", "|", "/"];
            let idx =
                ((self.session_start.elapsed().as_millis() / 125) % frames.len() as u128) as usize;
            let spinner = if sa.is_active { frames[idx] } else { "" };

            let turn_think = self
                .subagent_turn_thinking
                .get(&sa.name)
                .map(|s| s.as_str());
            let is_thinking = self
                .subagent_is_thinking
                .get(&sa.name)
                .copied()
                .unwrap_or_else(|| turn_think.map_or(!sa.thinking.is_empty(), |t| !t.is_empty()))
                && sa.is_active;
            let active_think = match turn_think {
                Some(t) => t,
                None => sa.thinking.as_str(),
            };

            let (bar_text, bar_style) = if is_thinking {
                let budget = self.get_thinking_budget(sa);
                let chars = active_think.len();
                let tok_count = chars.div_ceil(4);
                let remaining = budget.saturating_sub(tok_count);
                let text = format!(
                    " [Thinking: ~{} tokens ({} remaining) / {} chars] {}",
                    Self::format_count(tok_count),
                    Self::format_count(remaining),
                    Self::format_count(chars),
                    spinner
                );
                let fg_color = if remaining <= budget / 5 {
                    Color::LightRed
                } else {
                    Color::Yellow
                };
                (
                    text,
                    Style::default()
                        .bg(Color::DarkGray)
                        .fg(fg_color)
                        .add_modifier(Modifier::BOLD),
                )
            } else if sa.is_active && !sa.content.is_empty() {
                let text = format!(" [Status: Active - streaming output...] {}", spinner);
                (text, Style::default().bg(Color::DarkGray).fg(Color::Green))
            } else if sa.is_active {
                let text = format!(
                    " [Status: Active - waiting for model response...] {}",
                    spinner
                );
                (text, Style::default().bg(Color::DarkGray).fg(Color::Cyan))
            } else {
                (
                    " [Status: Idle]".to_string(),
                    Style::default().bg(Color::DarkGray).fg(Color::DarkGray),
                )
            };

            let status_p = Paragraph::new(bar_text)
                .style(bar_style)
                .alignment(ratatui::layout::Alignment::Left);
            frame.render_widget(status_p, status_area);
        }

        // F4: vertical scrollbar on the subagent-details pane.
        if max_scroll > 0 {
            let inner = Rect {
                x: details_area.x,
                y: details_area.y,
                width: details_area.width,
                height: details_area.height,
            };
            let mut scrollbar_state =
                ScrollbarState::new(max_scroll).position(self.subagent_scroll as usize);
            frame.render_stateful_widget(
                Scrollbar::new(ScrollbarOrientation::VerticalRight),
                inner,
                &mut scrollbar_state,
            );
        }

        frame.render_widget(subagent_block, area);
    }

    pub(crate) fn format_count(n: usize) -> String {
        if n >= 1_000_000 {
            format!("{:.1}M", n as f64 / 1_000_000.0)
        } else if n >= 1_000 {
            format!("{:.1}k", n as f64 / 1_000.0)
        } else {
            format!("{n}")
        }
    }

    pub(crate) fn format_token_counts(tokens_in: usize, tokens_out: usize) -> String {
        let total = tokens_in.saturating_add(tokens_out);
        format!(
            "{} in / {} out ({} total)",
            Self::format_count(tokens_in),
            Self::format_count(tokens_out),
            Self::format_count(total)
        )
    }

    pub(crate) fn render_status(&self, frame: &mut ratatui::Frame, area: Rect) {
        // F8: always surface the live running-subagent count, independent of
        // `status_line`, whenever any specialist is active.
        let active_agents: Vec<&str> = self
            .subagents
            .iter()
            .filter(|s| s.is_active)
            .map(|s| s.name.as_str())
            .collect();
        let mut status_str = self.status_line.clone();
        if active_agents.is_empty() {
            if let Some((prefix, _)) = status_str.split_once(". Active: ") {
                status_str = prefix.to_string();
            }
        } else {
            let count_suffix = format!(
                " [{} active: {}]",
                active_agents.len(),
                active_agents.join(", ")
            );
            status_str.push_str(&count_suffix);
        }
        // F3: animated activity indicator. When the status line indicates an
        // active phase, append a cycling suffix derived from `session_start`.
        let active_phase = [
            "Running",
            "Delegating",
            "calling backend",
            "calling model",
            "Starting",
            "streaming",
            "thinking",
            "Arbitrating",
        ]
        .iter()
        .any(|k| status_str.contains(k));
        if let Some(start) = self.waiting_for_token_since {
            let elapsed = start.elapsed().as_secs_f32();
            status_str.push_str(&format!(" ({:.1}s)", elapsed));
        }
        if active_phase {
            let frames = ["-", "\\", "|", "/"];
            let idx =
                ((self.session_start.elapsed().as_millis() / 125) % frames.len() as u128) as usize;
            status_str.push(' ');
            status_str.push_str(frames[idx]);
        }
        let (global_in, global_out) = crate::llm::get_global_token_counts();
        let tokens_in = self.tokens_in.max(global_in);
        let tokens_out = self.tokens_out.max(global_out);
        let tokens_str = Self::format_token_counts(tokens_in, tokens_out);
        let (status_text, status_style) = if self.is_abort_armed() {
            (
                " ⚠ [ABORT ARMED] Press ESC or Ctrl+C again to abort | Any other key to cancel "
                    .to_string(),
                Style::default()
                    .bg(Color::Red)
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            let left_text = if self.orchestrator_context_tokens > 0 {
                let ctx_str = Self::format_count(self.orchestrator_context_tokens);
                format!(
                    " Ctx: {} | Tokens: {} | Status: {}",
                    ctx_str, tokens_str, status_str
                )
            } else {
                format!(" Tokens: {} | Status: {}", tokens_str, status_str)
            };

            let right_text = if let Some(dur) = self.total_project_elapsed() {
                format!(
                    "Elapsed: {} ",
                    crate::orchestrator::workers::format_duration_human(dur.as_secs())
                )
            } else {
                String::new()
            };

            let total_width = area.width as usize;
            let left_w = UnicodeWidthStr::width(left_text.as_str());
            let right_w = UnicodeWidthStr::width(right_text.as_str());

            let status_text = if total_width > left_w + right_w {
                let padding = total_width - left_w - right_w;
                format!("{}{:width$}{}", left_text, "", right_text, width = padding)
            } else if total_width > right_w {
                let avail_left = total_width - right_w;
                let mut truncated = String::new();
                let mut cur_w = 0;
                for ch in left_text.chars() {
                    let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0);
                    if cur_w + ch_w > avail_left {
                        break;
                    }
                    truncated.push(ch);
                    cur_w += ch_w;
                }
                let padding = avail_left - cur_w;
                format!("{}{:width$}{}", truncated, "", right_text, width = padding)
            } else {
                left_text
            };

            (
                status_text,
                Style::default().bg(Color::DarkGray).fg(Color::White),
            )
        };
        let status_paragraph = Paragraph::new(status_text).style(status_style);
        frame.render_widget(status_paragraph, area);
    }

    pub(crate) fn render_input(&mut self, frame: &mut ratatui::Frame, area: Rect) {
        self.ensure_textarea_in_sync();

        let border_color = if self.is_abort_armed() {
            Color::Red
        } else {
            Color::Cyan
        };
        let input_block = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border_color))
            .title(" Input (Enter: Send, Alt/Shift+Enter: Newline, Ctrl+Z: Undo) ");

        self.textarea.set_block(input_block);
        self.textarea.set_cursor_line_style(Style::default());
        frame.render_widget(&self.textarea, area);
    }
}
