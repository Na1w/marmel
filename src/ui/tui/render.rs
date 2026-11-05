//! Ratatui frame drawing and panel rendering for TuiRenderer.

use super::TuiRenderer;
use super::formatting::*;
use super::theme;
use anyhow::Result;
use ratatui::Terminal;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::Rect;
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
        let think_style = thought_style();
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
                lines.push(Line::from(Span::styled(cleaned, theme::text_primary())));
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
        let steer_style = theme::role_manager();
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

    /// The transcript's largest legal offset — its **tail** — as the pass that
    /// draws the pane measured it.
    ///
    /// This is the one answer to "how far down may the transcript go", and it is
    /// the same number the pane drew and published (audit defects 11/12). The
    /// transcript counts its own wrapped lines at the width it draws them at —
    /// one column narrower once its scrollbar is needed — and reports
    /// `BodyReport::max_scroll`, which [`Self::draw_new_ui`] publishes into
    /// `transcript_measured_max` with the offset it drew. A scroll key, a resize
    /// clamp and the pane's own pin therefore all step from one geometry, and a
    /// press can no longer measure its step against an estimate the next frame
    /// disagrees with.
    ///
    /// The renderer's cached line estimate answers only for a transcript this
    /// process has never painted — a restore, a resize before the first frame, or
    /// a key press ahead of any draw — where there is no drawn geometry to agree
    /// with yet. It is a bootstrap, not a second owner: the moment a frame draws
    /// the pane, the measured number takes over and keeps the estimate out of the
    /// argument.
    pub(crate) fn transcript_tail(&self) -> u16 {
        match self.transcript_measured_max.get() {
            Some(measured) => measured.min(usize::from(u16::MAX)) as u16,
            None => self
                .estimated_chat_lines(self.chat_width.get())
                .saturating_sub(self.chat_height.get())
                .min(usize::from(u16::MAX)) as u16,
        }
    }

    /// The worker-detail tail, stated the same way as [`Self::transcript_tail`]:
    /// the maximum the Workers pane measured and published
    /// (`WorkerGeometry::detail_max`), and the wrapped-line estimate only while
    /// that pane has never been drawn.
    pub(crate) fn worker_detail_tail(&self) -> u16 {
        match self.detail_measured_max.get() {
            Some(measured) => measured.min(usize::from(u16::MAX)) as u16,
            None => self
                .estimated_subagent_lines(self.subagent_width.get())
                .saturating_sub(self.subagent_height.get())
                .min(usize::from(u16::MAX)) as u16,
        }
    }

    /// One frame's plan-box refresh: re-read the disk-backed plan text (cached to
    /// avoid synchronous I/O on every frame) and state the ONE scroll decision the
    /// plan box is allowed to make on its own — the first plan document this
    /// process has ever seen gets centred on the task in flight.
    ///
    /// Nothing else in the frame touches plan scroll state. Task traffic is not a
    /// centring event (`mod.rs::on_event` used to arm it on every
    /// `DelegationEvent::Started`, which silently undid an operator's scroll-up —
    /// audit defect 13), and a scroll gesture releases centring through
    /// [`Self::scroll_plan_to`] and is never handed back by a frame.
    fn refresh_plan_cache(&mut self) {
        let plan_path = std::path::Path::new(crate::manager::phase::MARMEL_DIR)
            .join(crate::manager::phase::PLAN_FILE);
        let archive_path = std::path::Path::new(crate::manager::phase::MARMEL_DIR)
            .join("execution_plan_archive.md");

        if !(self.last_plan_check.elapsed() >= std::time::Duration::from_millis(250)
            || self.plan_content.is_empty())
        {
            return;
        }

        self.last_plan_check = std::time::Instant::now();
        let current_plan_mtime = std::fs::metadata(&plan_path)
            .ok()
            .and_then(|m| m.modified().ok());
        let current_archive_mtime = std::fs::metadata(&archive_path)
            .ok()
            .and_then(|m| m.modified().ok());

        if current_plan_mtime == self.plan_mtime
            && current_archive_mtime == self.archive_mtime
            && !self.plan_content.is_empty()
        {
            return;
        }

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

        // A plan this process has not seen before opens its pane and gets centred;
        // the transition is the only writer of the centring flag outside a scroll
        // gesture and a restore, and it is an edge — `had_active_plan` is true from
        // here on, so no later task traffic re-arms it.
        if (has_active_plan || is_archived) && !self.had_active_plan {
            self.arm_plan_centring();
        }
        self.had_active_plan = has_active_plan || is_archived;
    }

    pub(crate) fn draw(
        &mut self,
        terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    ) -> Result<()> {
        self.refresh_plan_cache();

        // No scroll state is written before the frame is drawn. Every pane clamps,
        // pins and reports its own geometry while it paints (audit defects 11/12:
        // this pass used to re-pin `chat_scroll` from the follow mirror and
        // re-measure the detail half from a line-count estimate, in parallel with
        // the components, which erased any scroll that did not also flip a flag and
        // let a key step and a frame disagree about the geometry). The single frame
        // write-back in `draw_new_ui` publishes what each pane drew, and the follow
        // flags are carried to the panes as the requests they are
        // (`TranscriptData::follow`, `PlanData::auto_scroll`,
        // `ActivityData::autoscroll`) rather than being resolved into offsets here.
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The streaming buffers used to carry their own hard-coded copies of the
    /// "thought", "body" and "steer" styles (§B.3), which competed with the
    /// palette. They now ask the palette for the same role tokens the panes use,
    /// and the body tier keeps **no** explicit foreground (§F2: a forced `White`
    /// is white-on-white on a light profile).
    #[test]
    fn the_streaming_buffers_are_styled_by_the_palette_slots() {
        let mut r = TuiRenderer::new();
        r.current_thought = "weigh the seam".to_string();
        r.current_content = "chunk of orchestrator".to_string();
        r.steer_sentence_buffer = "hold the fixture".to_string();

        let thought = r.render_streaming_thought();
        assert_eq!(thought.len(), 1, "got: {thought:?}");
        assert_eq!(thought[0].spans[0].style, thought_style());

        let content = r.render_streaming_content();
        assert_eq!(content.len(), 1, "got: {content:?}");
        assert_eq!(content[0].spans[0].style, theme::text_primary());
        assert_eq!(
            content[0].spans[0].style.fg, None,
            "body text inherits the operator's terminal profile"
        );

        let steer = r.render_streaming_steer();
        assert_eq!(steer.len(), 1, "got: {steer:?}");
        assert_eq!(steer[0].spans[0].style, theme::role_manager());
    }
}
