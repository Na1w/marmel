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

/// The shared stream sink that both Manager and specialist turns write demuxed events into.
pub trait StreamSink {
    /// Deliver one demuxed stream event to the channel.
    fn emit(&mut self, event: StreamEvent);

    /// Query whether the consumer has requested an early abort.
    fn is_aborted(&mut self) -> bool {
        false
    }
}

/// A sink that discards every event.
#[derive(Debug, Default, Clone)]
pub struct NullSink;

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
        }
    }
}

impl StreamConfig {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            model: cfg.model.clone(),
            temperature: cfg.temperature,
            top_p: cfg.top_p,
            frequency_penalty: cfg.frequency_penalty,
            presence_penalty: cfg.presence_penalty,
            preserve_thinking: cfg.preserve_thinking,
            recovery: false,
            mcp_servers: cfg.orchestration.mcp_servers.clone(),
        }
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
    let mut tools = crate::types::ToolDef::default_tools();
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

pub async fn chat_client_turn<S>(
    client: &ChatClient,
    messages: Vec<Message>,
    cfg: &StreamConfig,
    sink: &mut S,
) -> Result<Message>
where
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

        let mut demux = ThinkingDemuxer::with_preserve(cfg.preserve_thinking);
        let reply = client
            .chat_stream(&req, |delta| {
                if !delta.is_empty() {
                    demux.push_delta(delta, |kind, text| match kind {
                        DeltaKind::Content => sink.emit(StreamEvent::Content(text.to_string())),
                        DeltaKind::Thinking => sink.emit(StreamEvent::Thinking(text.to_string())),
                    });
                }
                !sink.is_aborted()
            })
            .await?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_llm_stream_channel_demuxes_thinking() {
        let messages = vec![Message::User {
            content: "hi".to_string(),
        }];
        let cfg = StreamConfig::default();
        let mut sink = VecSink::default();

        let raw_payload = "[thinking]Let me reason[/thinking]Hello world!";
        let msg = drive_streamed_turn(
            |_req| async move {
                Ok(StreamedReply {
                    content: "Hello world!".to_string(),
                    reasoning: "Let me reason".to_string(),
                    raw: raw_payload.to_string(),
                    tool_calls: vec![],
                })
            },
            messages,
            &cfg,
            &mut sink,
        )
        .await
        .unwrap();

        assert_eq!(sink.thinking(), "Let me reason");
        assert_eq!(sink.content(), "Hello world!");

        match msg {
            Message::Assistant {
                content,
                reasoning_content,
                ..
            } => {
                assert_eq!(content.as_deref(), Some("Hello world!"));
                assert_eq!(reasoning_content.as_deref(), Some("Let me reason"));
            }
            _ => panic!("expected assistant message"),
        }
    }
}
