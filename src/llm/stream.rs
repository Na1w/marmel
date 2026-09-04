//! Shared stream channel for both Manager and specialist turns.

use crate::types::{ChatRequest, Message};
use anyhow::Result;
use std::future::Future;

use super::client::{ChatClient, StreamedReply};
use super::thinking::{
    DeltaKind, NudgePolicy, RecoveryAdjustment, ThinkingDemuxer, apply_recovery,
};

/// A single demuxed stream event routed to the shared channel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamEvent {
    /// A chunk of visible assistant content.
    Content(String),
    /// A chunk of reasoning / thinking-channel content.
    Thinking(String),
    /// A status note (retry, nudge, context compaction, …).
    Status(String),
}

/// Control signal polled from a `StreamSink` on delta boundaries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StreamControl {
    /// Continue streaming chunks.
    Continue,
    /// Abort the stream completely.
    Abort,
    /// Pause the active stream immediately to handle mid-flight user steering.
    Pause { user_input: String },
}

/// Action to take after handling a paused stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PauseAction {
    /// Resume the interrupted stream from where it was paused.
    Resume,
    /// Abort the turn and do not resume.
    Abort,
}

/// The shared stream sink that both Manager and specialist turns write demuxed events into.
#[async_trait::async_trait]
pub trait StreamSink: Send {
    /// Deliver one demuxed stream event to the channel.
    fn emit(&mut self, event: StreamEvent);

    /// Query whether the consumer has requested an early abort.
    fn is_aborted(&mut self) -> bool {
        false
    }

    /// Query stream control state on delta boundaries.
    fn poll_control(&mut self) -> StreamControl {
        if self.is_aborted() {
            StreamControl::Abort
        } else {
            StreamControl::Continue
        }
    }

    /// Callback invoked when a stream was paused due to `StreamControl::Pause`.
    /// Returns whether to resume the stream or abort.
    async fn on_pause(&mut self, user_input: &str) -> PauseAction {
        let _ = user_input;
        PauseAction::Resume
    }
}

/// A sink that discards every event.
#[derive(Debug, Default, Clone)]
pub struct NullSink;

#[async_trait::async_trait]
impl StreamSink for NullSink {
    fn emit(&mut self, _event: StreamEvent) {}
}

/// A sink that buffers every event, for tests and transcript inspection.
#[derive(Debug, Default, Clone)]
pub struct VecSink {
    pub events: Vec<StreamEvent>,
}

impl VecSink {
    pub fn content(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Content(c) => Some(c.as_str()),
                _ => None,
            })
            .collect()
    }

    pub fn thinking(&self) -> String {
        self.events
            .iter()
            .filter_map(|e| match e {
                StreamEvent::Thinking(c) => Some(c.as_str()),
                _ => None,
            })
            .collect()
    }
}

#[async_trait::async_trait]
impl StreamSink for VecSink {
    fn emit(&mut self, event: StreamEvent) {
        self.events.push(event);
    }
}

/// Tunables for a single streamed turn.
#[derive(Debug, Clone)]
pub struct StreamConfig {
    pub model: String,
    pub temperature: f32,
    pub top_p: f32,
    pub frequency_penalty: f32,
    pub presence_penalty: f32,
    pub preserve_thinking: bool,
    pub recovery: bool,
    pub mcp_servers: Vec<String>,
    pub repetition_threshold: usize,
    pub min_pattern_len: usize,
    pub max_stream_tokens: usize,
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            temperature: 0.7,
            top_p: 0.9,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
            preserve_thinking: false,
            recovery: false,
            mcp_servers: Vec::new(),
            repetition_threshold: 5,
            min_pattern_len: 5,
            max_stream_tokens: 32768,
        }
    }
}

impl StreamConfig {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        let default_mon = crate::config::MonitoringConfig::default();
        let mon = cfg.monitoring.as_ref().unwrap_or(&default_mon);
        Self {
            model: cfg.model.clone(),
            temperature: cfg.temperature,
            top_p: cfg.top_p,
            frequency_penalty: cfg.frequency_penalty,
            presence_penalty: cfg.presence_penalty,
            preserve_thinking: cfg.preserve_thinking,
            recovery: false,
            mcp_servers: cfg.orchestration.mcp_servers.clone(),
            repetition_threshold: mon.repetition_threshold,
            min_pattern_len: mon.min_pattern_len,
            max_stream_tokens: mon.max_stream_tokens,
        }
    }
}

/// Target destination for demuxed stream events.
/// Target destination for demuxed stream events.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StreamTarget {
    /// Emit events to the orchestrator event bus (used by specialists and validators).
    OrchestratorEvents,
    /// Emit events to a StreamSink (used by chat_client_turn).
    Sink,
}

/// Helper that manages token budget capping, thinking demuxing, repetition detection,
/// and cancellation checking for a single streaming turn.
pub struct TurnStreamHandler<'a> {
    pub max_tokens: usize,
    pub tokens_count: usize,
    pub budget_exceeded: bool,
    pub rep_triggered: bool,
    pub demux: ThinkingDemuxer,
    pub rep_detector: &'a mut crate::harness::monitor::RepetitionDetector,
    pub cancellation_token: Option<&'a tokio_util::sync::CancellationToken>,
    pub pause_requested: Option<String>,
}

impl<'a> TurnStreamHandler<'a> {
    /// Create a stream handler targeting orchestrator events with a cancellation token.
    pub fn new(
        max_tokens: usize,
        rep_detector: &'a mut crate::harness::monitor::RepetitionDetector,
        cancellation_token: &'a tokio_util::sync::CancellationToken,
    ) -> Self {
        Self::with_cancellation(max_tokens, rep_detector, Some(cancellation_token), false)
    }

    /// Create a stream handler for a StreamSink.
    pub fn for_sink(
        max_tokens: usize,
        rep_detector: &'a mut crate::harness::monitor::RepetitionDetector,
        preserve_thinking: bool,
    ) -> Self {
        Self::with_cancellation(max_tokens, rep_detector, None, preserve_thinking)
    }

    /// Create a stream handler with explicit cancellation and configuration.
    pub fn with_cancellation(
        max_tokens: usize,
        rep_detector: &'a mut crate::harness::monitor::RepetitionDetector,
        cancellation_token: Option<&'a tokio_util::sync::CancellationToken>,
        preserve_thinking: bool,
    ) -> Self {
        Self {
            max_tokens: max_tokens.max(256),
            tokens_count: 0,
            budget_exceeded: false,
            rep_triggered: false,
            demux: ThinkingDemuxer::with_preserve(preserve_thinking),
            rep_detector,
            cancellation_token,
            pause_requested: None,
        }
    }

    /// Take any pending pause request triggered by mid-flight user steering.
    pub fn take_pause_request(&mut self) -> Option<String> {
        self.pause_requested.take()
    }

    /// Process an incoming chunk delta targeting orchestrator events. Returns true to continue streaming, false to cut stream.
    pub fn on_chunk(&mut self, chunk: &str) -> bool {
        if !chunk.is_empty() {
            self.tokens_count += 1;
            if self.tokens_count > self.max_tokens {
                self.budget_exceeded = true;
            }
        }
        let rep_det = &mut *self.rep_detector;
        let rep_trig = &mut self.rep_triggered;
        self.demux.push_delta(chunk, |kind, text| {
            rep_det.push(text);
            if rep_det.is_repeating() {
                *rep_trig = true;
            }
            match kind {
                DeltaKind::Content => {
                    crate::orchestrator::emit_event(crate::ui::Event::Message(text.to_string()));
                }
                DeltaKind::Thinking => {
                    crate::orchestrator::emit_event(crate::ui::Event::Thinking(text.to_string()));
                }
            }
        });

        let cancelled = self
            .cancellation_token
            .map(|t| t.is_cancelled())
            .unwrap_or(false);

        !cancelled
            && !self.rep_triggered
            && !self.budget_exceeded
            && !crate::orchestrator::is_globally_cancelled()
    }

    /// Process an incoming chunk delta delivering events to `sink`. Returns true to continue streaming, false to cut stream.
    pub fn on_chunk_with_sink<S: StreamSink + ?Sized>(
        &mut self,
        chunk: &str,
        sink: &mut S,
    ) -> bool {
        if !chunk.is_empty() {
            self.tokens_count += 1;
            if self.tokens_count > self.max_tokens {
                self.budget_exceeded = true;
            }
        }
        let rep_det = &mut *self.rep_detector;
        let rep_trig = &mut self.rep_triggered;
        self.demux.push_delta(chunk, |kind, text| {
            rep_det.push(text);
            if rep_det.is_repeating() {
                *rep_trig = true;
            }
            match kind {
                DeltaKind::Content => sink.emit(StreamEvent::Content(text.to_string())),
                DeltaKind::Thinking => sink.emit(StreamEvent::Thinking(text.to_string())),
            }
        });

        match sink.poll_control() {
            StreamControl::Abort => return false,
            StreamControl::Pause { user_input } => {
                self.pause_requested = Some(user_input);
                return false;
            }
            StreamControl::Continue => {}
        }

        let cancelled = self
            .cancellation_token
            .map(|t| t.is_cancelled())
            .unwrap_or(false);

        !cancelled
            && !self.rep_triggered
            && !self.budget_exceeded
            && !crate::orchestrator::is_globally_cancelled()
    }

    /// Flushes any pending thinking/content deltas to orchestrator events.
    pub fn finish(&mut self) {
        self.demux.finish_delta(|kind, text| match kind {
            DeltaKind::Content => {
                crate::orchestrator::emit_event(crate::ui::Event::Message(text.to_string()));
            }
            DeltaKind::Thinking => {
                crate::orchestrator::emit_event(crate::ui::Event::Thinking(text.to_string()));
            }
        });
    }

    /// Flushes any pending thinking/content deltas to `sink`.
    pub fn finish_with_sink<S: StreamSink + ?Sized>(&mut self, sink: &mut S) {
        self.demux.finish_delta(|kind, text| match kind {
            DeltaKind::Content => sink.emit(StreamEvent::Content(text.to_string())),
            DeltaKind::Thinking => sink.emit(StreamEvent::Thinking(text.to_string())),
        });
    }

    /// Consumes the handler and produces the demuxed assistant Message.
    pub fn into_message(self) -> Message {
        self.demux.into_message()
    }
}

/// Drive one streamed turn through the shared channel and return the final assistant Message.
pub async fn drive_streamed_turn<F, Fut, S>(
    mut chat: F,
    messages: Vec<Message>,
    cfg: &StreamConfig,
    sink: &mut S,
) -> Result<Message>
where
    F: FnMut(ChatRequest) -> Fut,
    Fut: Future<Output = Result<StreamedReply>>,
    S: StreamSink + ?Sized,
{
    let mut transcript = messages;
    let nudge = NudgePolicy::default();
    let mut empty_attempts = 0u32;

    let mut recovery = cfg.recovery;
    loop {
        let req = build_request(cfg, transcript.clone());
        let req = if recovery {
            recovery = false;
            apply_recovery(&req, RecoveryAdjustment::default())
        } else {
            req
        };

        let reply = chat(req).await?;

        let mut demux = ThinkingDemuxer::with_preserve(cfg.preserve_thinking);
        demux.push_delta(&reply.raw, |kind, text| match kind {
            DeltaKind::Content => sink.emit(StreamEvent::Content(text.to_string())),
            DeltaKind::Thinking => sink.emit(StreamEvent::Thinking(text.to_string())),
        });
        demux.finish_delta(|kind, text| match kind {
            DeltaKind::Content => sink.emit(StreamEvent::Content(text.to_string())),
            DeltaKind::Thinking => sink.emit(StreamEvent::Thinking(text.to_string())),
        });

        let mut assistant = demux.into_message();
        if let Message::Assistant {
            tool_calls,
            content,
            ..
        } = &mut assistant
        {
            tool_calls.extend(reply.tool_calls);
            if tool_calls.is_empty()
                && let Some(text) = content
            {
                let monitor = crate::harness::monitor::HarnessMonitor::with_new_stats();
                let rescued = monitor.rescue_xml(text);
                if !rescued.is_empty() {
                    *tool_calls = rescued;
                }
            }
        }

        if is_empty_production(&assistant) && nudge.should_nudge(empty_attempts) {
            empty_attempts += 1;
            sink.emit(StreamEvent::Status(format!(
                "empty production — nudge {empty_attempts}/{}",
                nudge.max_attempts()
            )));
            transcript.push(assistant);
            transcript = nudge.nudge(transcript);
            continue;
        }

        transcript.push(assistant);
        return Ok(transcript.pop().expect("assistant just pushed"));
    }
}

fn is_empty_production(m: &Message) -> bool {
    match m {
        Message::Assistant {
            content,
            tool_calls,
            ..
        } => content.as_deref().is_none_or(str::is_empty) && tool_calls.is_empty(),
        _ => false,
    }
}

pub(crate) fn build_request(cfg: &StreamConfig, messages: Vec<Message>) -> ChatRequest {
    let mut tools = crate::types::ToolDef::manager_tools();
    if let Some(mcp) = crate::harness::get_mcp_manager() {
        for tool in mcp.tools_for_servers(&cfg.mcp_servers) {
            tools.push(crate::types::ToolDef::from_mcp(&tool));
        }
    }

    ChatRequest {
        model: cfg.model.clone(),
        messages,
        temperature: Some(cfg.temperature),
        top_p: Some(cfg.top_p),
        frequency_penalty: Some(cfg.frequency_penalty),
        presence_penalty: Some(cfg.presence_penalty),
        stream: Some(true),
        enable_thinking: None,
        tools: Some(tools),
    }
}

pub(crate) fn build_continuation_request(
    original_req: &ChatRequest,
    base_messages: &[Message],
    accumulated_content: &str,
    accumulated_thinking: &str,
) -> ChatRequest {
    let mut req = original_req.clone();
    let mut messages = base_messages.to_vec();
    if !accumulated_content.is_empty() || !accumulated_thinking.is_empty() {
        messages.push(Message::Assistant {
            content: if accumulated_content.is_empty() {
                None
            } else {
                Some(accumulated_content.to_string())
            },
            reasoning_content: if accumulated_thinking.is_empty() {
                None
            } else {
                Some(accumulated_thinking.to_string())
            },
            tool_calls: Vec::new(),
        });
    }
    req.messages = messages;
    req
}

pub(crate) fn build_fallback_continuation_request(
    original_req: &ChatRequest,
    base_messages: &[Message],
    accumulated_content: &str,
) -> ChatRequest {
    let mut req = original_req.clone();
    let mut messages = base_messages.to_vec();
    messages.push(Message::Assistant {
        content: Some(accumulated_content.to_string()),
        reasoning_content: None,
        tool_calls: Vec::new(),
    });
    messages.push(Message::User {
        content: "(System continuation notice: continue generating your response directly from where you were interrupted. Do not repeat anything.)".to_string(),
    });
    req.messages = messages;
    req
}

/// Output from a resumable streaming LLM turn.
#[derive(Debug, Clone)]
pub struct ResumableStreamOutput {
    pub reply: StreamedReply,
    pub budget_exceeded: bool,
    pub rep_triggered: bool,
    pub was_aborted_by_steer: bool,
}

/// Drives a resumable streaming chat call against `client`, supporting mid-flight pause,
/// steering arbitration, and continuation prefill.
pub async fn chat_stream_resumable<S>(
    client: &ChatClient,
    req: &ChatRequest,
    sink: &mut S,
    max_tokens: usize,
    rep_detector: &mut crate::harness::monitor::RepetitionDetector,
    preserve_thinking: bool,
    cancellation_token: Option<&tokio_util::sync::CancellationToken>,
) -> Result<ResumableStreamOutput>
where
    S: StreamSink + ?Sized,
{
    let mut stream_handler = TurnStreamHandler::with_cancellation(
        max_tokens,
        rep_detector,
        cancellation_token,
        preserve_thinking,
    );

    let mut current_req = req.clone();
    let base_messages = req.messages.clone();
    let mut all_tool_calls = Vec::new();
    let mut was_aborted_by_steer = false;

    loop {
        let reply_res = client
            .chat_stream(&current_req, |delta| {
                stream_handler.on_chunk_with_sink(delta, sink)
            })
            .await;

        let reply = match reply_res {
            Ok(r) => r,
            Err(e) => {
                if current_req.messages.len() > base_messages.len() {
                    let fallback_req = build_fallback_continuation_request(
                        &current_req,
                        &base_messages,
                        stream_handler.demux.content(),
                    );
                    match client
                        .chat_stream(&fallback_req, |delta| {
                            stream_handler.on_chunk_with_sink(delta, sink)
                        })
                        .await
                    {
                        Ok(r) => r,
                        Err(_) => return Err(e),
                    }
                } else {
                    return Err(e);
                }
            }
        };

        all_tool_calls.extend(reply.tool_calls);

        if let Some(user_input) = stream_handler.take_pause_request() {
            crate::debug_log::log_stream_pause(
                &current_req.model,
                &user_input,
                stream_handler.tokens_count,
            );
            match sink.on_pause(&user_input).await {
                PauseAction::Resume => {
                    crate::debug_log::log_stream_resume(
                        &current_req.model,
                        stream_handler.demux.content().len(),
                    );
                    current_req = build_continuation_request(
                        &current_req,
                        &base_messages,
                        stream_handler.demux.content(),
                        stream_handler.demux.thinking(),
                    );
                    continue;
                }
                PauseAction::Abort => {
                    crate::debug_log::log_stream_abort(
                        &current_req.model,
                        "aborted by steer arbitrator during pause",
                    );
                    was_aborted_by_steer = true;
                    break;
                }
            }
        }

        break;
    }

    stream_handler.finish_with_sink(sink);

    let budget_exceeded = stream_handler.budget_exceeded;
    let rep_triggered = stream_handler.rep_triggered;

    let final_content = stream_handler.demux.content().to_string();
    let final_thinking = stream_handler.demux.thinking().to_string();

    Ok(ResumableStreamOutput {
        reply: StreamedReply {
            content: final_content,
            reasoning: final_thinking,
            raw: String::new(),
            tool_calls: all_tool_calls,
        },
        budget_exceeded,
        rep_triggered,
        was_aborted_by_steer,
    })
}

pub async fn chat_client_turn<S>(
    client: &ChatClient,
    messages: Vec<Message>,
    cfg: &StreamConfig,
    sink: &mut S,
) -> Result<Message>
where
    S: StreamSink,
{
    let mut transcript = messages;
    let nudge = NudgePolicy::default();
    let mut empty_attempts = 0u32;
    let mut recovery = cfg.recovery;

    loop {
        let req = build_request(cfg, transcript.clone());
        let req = if recovery {
            recovery = false;
            apply_recovery(&req, RecoveryAdjustment::default())
        } else {
            req
        };

        let max_tokens = cfg.max_stream_tokens.max(256);
        let mut rep_detector = crate::harness::monitor::RepetitionDetector::new(
            cfg.repetition_threshold,
            cfg.min_pattern_len,
        );

        let out = chat_stream_resumable(
            client,
            &req,
            sink,
            max_tokens,
            &mut rep_detector,
            cfg.preserve_thinking,
            None,
        )
        .await?;

        let budget_exceeded = out.budget_exceeded;
        let rep_triggered = out.rep_triggered;
        let was_aborted_by_steer = out.was_aborted_by_steer;

        let mut assistant = Message::Assistant {
            content: if out.reply.content.is_empty() {
                None
            } else {
                Some(out.reply.content.clone())
            },
            reasoning_content: if out.reply.reasoning.is_empty() {
                None
            } else {
                Some(out.reply.reasoning.clone())
            },
            tool_calls: out.reply.tool_calls.clone(),
        };

        if let Message::Assistant {
            tool_calls,
            content,
            ..
        } = &mut assistant
            && tool_calls.is_empty()
            && let Some(text) = content
        {
            let monitor = crate::harness::monitor::HarnessMonitor::with_new_stats();
            let rescued = monitor.rescue_xml(text);
            if !rescued.is_empty() {
                *tool_calls = rescued;
            }
        }

        if was_aborted_by_steer {
            return Ok(assistant);
        }

        let has_tools = match &assistant {
            Message::Assistant { tool_calls, .. } => !tool_calls.is_empty(),
            _ => false,
        };

        if budget_exceeded && !has_tools && empty_attempts < nudge.max_attempts() {
            empty_attempts += 1;
            tracing::warn!(
                "Stream terminated due to output token budget ({max_tokens}) — injecting budget recovery nudge ({}/{})",
                empty_attempts,
                nudge.max_attempts()
            );
            sink.emit(StreamEvent::Status(format!(
                "output budget reached ({max_tokens} tokens) — nudge {empty_attempts}/{}",
                nudge.max_attempts()
            )));
            transcript.push(Message::Assistant {
                content: Some(format!(
                    "[Generation truncated: exceeded {max_tokens} token single-turn limit]"
                )),
                reasoning_content: None,
                tool_calls: Vec::new(),
            });
            transcript.push(Message::User {
                content: format!(
                    "SYSTEM NOTICE: Your response exceeded the single-turn output budget limit ({max_tokens} tokens) and was truncated. Please be concise and execute your necessary tools (such as `delegate_task` or `create_plan`) now."
                ),
            });
            recovery = true;
            continue;
        }

        if rep_triggered && !has_tools && empty_attempts < nudge.max_attempts() {
            empty_attempts += 1;
            tracing::warn!(
                "Stream terminated due to repetitive loop — injecting repetition nudge ({}/{})",
                empty_attempts,
                nudge.max_attempts()
            );
            sink.emit(StreamEvent::Status(format!(
                "repetition loop recovered — nudge {empty_attempts}/{}",
                nudge.max_attempts()
            )));
            transcript.push(Message::Assistant {
                content: Some("[Generation interrupted due to repetitive loop]".to_string()),
                reasoning_content: None,
                tool_calls: Vec::new(),
            });
            transcript.push(Message::User {
                content: "SYSTEM NOTICE: Repetitive generation loop detected. Terminate conversational debate immediately and call your required tools (such as `delegate_task` or `create_plan`) now.".to_string(),
            });
            recovery = true;
            continue;
        }

        if is_empty_production(&assistant) && nudge.should_nudge(empty_attempts) {
            empty_attempts += 1;
            sink.emit(StreamEvent::Status(format!(
                "empty production — nudge {empty_attempts}/{}",
                nudge.max_attempts()
            )));
            transcript.push(assistant);
            transcript = nudge.nudge(transcript);
            continue;
        }

        transcript.push(assistant);
        return Ok(transcript.pop().expect("assistant just pushed"));
    }
}

#[cfg(test)]
#[path = "stream_tests.rs"]
mod tests;
