//! Reqwest SSE chat client with retry and timeout watchdogs.

use crate::types::{ChatChunk, ChatRequest};
use anyhow::Result;
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use std::time::Duration;
use thiserror::Error;

/// First SSE event must arrive within this window or the request fails.
pub const INITIAL_RESPONSE_WATCHDOG_SECS: u64 = 60;
/// Upper bound on the entire streaming read.
pub const OVERALL_READ_TIMEOUT_SECS: u64 = 1800;
/// Maximum total attempts (initial + up to 2 retries for 503/429).
pub const MAX_ATTEMPTS: u32 = 3;
/// Backoff base: sleep = `BACKOFF_BASE_MS × attempt`.
pub const BACKOFF_BASE_MS: u64 = 1000;

static GLOBAL_TOKENS_IN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static GLOBAL_TOKENS_OUT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

pub fn record_tokens_in(count: usize) {
    GLOBAL_TOKENS_IN.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
}

pub fn record_tokens_out(count: usize) {
    GLOBAL_TOKENS_OUT.fetch_add(count as u64, std::sync::atomic::Ordering::Relaxed);
}

pub fn get_global_token_counts() -> (usize, usize) {
    (
        GLOBAL_TOKENS_IN.load(std::sync::atomic::Ordering::Relaxed) as usize,
        GLOBAL_TOKENS_OUT.load(std::sync::atomic::Ordering::Relaxed) as usize,
    )
}

fn count_reply_tokens(
    content: &str,
    reasoning: &str,
    tool_calls: &[crate::types::ToolCall],
) -> usize {
    let enc = tiktoken_rs::cl100k_base_singleton();
    enc.encode_ordinary(content).len()
        + enc.encode_ordinary(reasoning).len()
        + tool_calls
            .iter()
            .map(|tc| {
                1 + enc.encode_ordinary(&tc.function.name).len()
                    + enc.encode_ordinary(&tc.function.arguments).len()
            })
            .sum::<usize>()
}

/// A single fully-assembled assistant reply chunk sequence.
#[derive(Debug, Clone, Default)]
pub struct StreamedReply {
    pub content: String,
    pub reasoning: String,
    pub raw: String,
    pub tool_calls: Vec<crate::types::ToolCall>,
}

/// Streaming chat client bound to a backend.
#[derive(Debug, Clone)]
pub struct ChatClient {
    backend_url: String,
    auth_token: String,
    model: String,
}

#[derive(Debug, Error)]
pub(crate) enum ChatError {
    #[error("backend returned HTTP {status}: {body}")]
    HttpStatus { status: u16, body: String },
    #[error("first event did not arrive within {INITIAL_RESPONSE_WATCHDOG_SECS}s")]
    InitialTimeout,
    #[error("stream exceeded {OVERALL_READ_TIMEOUT_SECS}s read timeout")]
    ReadTimeout,
    #[error("SSE stream error: {0}")]
    Stream(String),
    #[error("transport error: {0}")]
    Transport(String),
}

impl ChatError {
    fn is_retryable(&self) -> bool {
        matches!(
            self,
            ChatError::HttpStatus {
                status: 503 | 429,
                ..
            }
        )
    }
}

impl ChatClient {
    pub fn from_config(cfg: &crate::config::Config) -> Self {
        Self {
            backend_url: cfg.backend_url.clone(),
            auth_token: cfg.auth_token.clone(),
            model: cfg.model.clone(),
        }
    }

    pub fn new(backend_url: impl Into<String>, model: impl Into<String>) -> Self {
        Self {
            backend_url: backend_url.into(),
            auth_token: String::new(),
            model: model.into(),
        }
    }

    pub fn new_with_token(
        backend_url: impl Into<String>,
        model: impl Into<String>,
        auth_token: impl Into<String>,
    ) -> Self {
        Self {
            backend_url: backend_url.into(),
            auth_token: auth_token.into(),
            model: model.into(),
        }
    }

    pub async fn chat(&self, req: &ChatRequest) -> Result<StreamedReply> {
        self.chat_stream(req, |_| true).await
    }

    pub async fn chat_stream<F>(&self, req: &ChatRequest, mut on_delta: F) -> Result<StreamedReply>
    where
        F: FnMut(&str) -> bool,
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.try_chat_once(req, &mut on_delta).await {
                Ok(reply) => return Ok(reply),
                Err(e) if e.is_retryable() && attempt < MAX_ATTEMPTS => {
                    let ms = BACKOFF_BASE_MS * attempt as u64;
                    tokio::time::sleep(Duration::from_millis(ms)).await;
                }
                Err(e) => return Err(anyhow::anyhow!("{e}")),
            }
        }
    }

    async fn try_chat_once<F>(
        &self,
        req: &ChatRequest,
        on_delta: &mut F,
    ) -> Result<StreamedReply, ChatError>
    where
        F: FnMut(&str) -> bool,
    {
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| ChatError::Transport(e.to_string()))?;

        let url = format!(
            "{}/chat/completions",
            self.backend_url.trim_end_matches('/')
        );

        let mut req_body = req.clone();
        req_body.stream = Some(true);
        if req_body.model.is_empty() {
            req_body.model = self.model.clone();
        }

        tracing::info!(
            "Calling LLM backend at {} (model: {}, messages: {})",
            url,
            req_body.model,
            req_body.messages.len()
        );
        let prompt_tokens = crate::agent::context::count_tokens(&req_body.messages);
        record_tokens_in(prompt_tokens);
        tracing::debug!(
            "LLM request body: {}",
            serde_json::to_string(&req_body).unwrap_or_default()
        );

        let mut builder = client.post(&url).json(&req_body);
        if !self.auth_token.is_empty() {
            builder = builder.bearer_auth(&self.auth_token);
        }

        let send_fut = builder.send();
        tokio::pin!(send_fut);
        let resp = loop {
            if !on_delta("") {
                return Ok(StreamedReply::default());
            }
            match tokio::time::timeout(Duration::from_millis(50), &mut send_fut).await {
                Ok(res) => break res.map_err(|e| ChatError::Transport(e.to_string()))?,
                Err(_) => {
                    if !on_delta("") {
                        return Ok(StreamedReply::default());
                    }
                }
            }
        };

        if !resp.status().is_success() {
            let status = resp.status().as_u16();
            let body = resp.text().await.unwrap_or_default();
            tracing::error!("LLM backend returned HTTP {status}: {body}");
            return Err(ChatError::HttpStatus { status, body });
        }

        let mut stream = resp.bytes_stream().eventsource();

        let mut content = String::new();
        let mut reasoning = String::new();
        let mut raw = String::new();
        let mut tool_calls_map =
            std::collections::BTreeMap::<usize, (Option<String>, String, String)>::new();
        let mut in_reasoning = false;

        let first_start = std::time::Instant::now();
        let first = loop {
            if !on_delta("") {
                return Ok(StreamedReply::default());
            }
            if first_start.elapsed() >= Duration::from_secs(INITIAL_RESPONSE_WATCHDOG_SECS) {
                return Err(ChatError::InitialTimeout);
            }
            match tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
                Ok(res) => break res,
                Err(_) => {
                    if !on_delta("") {
                        return Ok(StreamedReply::default());
                    }
                }
            }
        };

        if let Some(ev) = first {
            let ev = ev.map_err(|e| ChatError::Stream(e.to_string()))?;
            if consume_event(
                &ev,
                &mut content,
                &mut reasoning,
                &mut raw,
                &mut tool_calls_map,
                &mut in_reasoning,
                on_delta,
            )? {
                if in_reasoning {
                    let _ = on_delta("</think>");
                }
                let tool_calls = map_to_tool_calls(tool_calls_map);
                let reply = StreamedReply {
                    content,
                    reasoning,
                    raw,
                    tool_calls,
                };
                let out_toks =
                    count_reply_tokens(&reply.content, &reply.reasoning, &reply.tool_calls);
                record_tokens_out(out_toks);
                return Ok(reply);
            }
        } else {
            let tool_calls = map_to_tool_calls(tool_calls_map);
            let reply = StreamedReply {
                content,
                reasoning,
                raw,
                tool_calls,
            };
            let out_toks = count_reply_tokens(&reply.content, &reply.reasoning, &reply.tool_calls);
            record_tokens_out(out_toks);
            return Ok(reply);
        }

        let consume = async {
            loop {
                if !on_delta("") {
                    break;
                }
                match tokio::time::timeout(Duration::from_millis(50), stream.next()).await {
                    Ok(Some(ev)) => {
                        let ev = ev.map_err(|e| ChatError::Stream(e.to_string()))?;
                        if consume_event(
                            &ev,
                            &mut content,
                            &mut reasoning,
                            &mut raw,
                            &mut tool_calls_map,
                            &mut in_reasoning,
                            on_delta,
                        )? {
                            break;
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {
                        if !on_delta("") {
                            break;
                        }
                    }
                }
            }
            if in_reasoning {
                in_reasoning = false;
                let _ = on_delta("</think>");
            }
            Ok::<(), ChatError>(())
        };
        tokio::time::timeout(Duration::from_secs(OVERALL_READ_TIMEOUT_SECS), consume)
            .await
            .map_err(|_| ChatError::ReadTimeout)??;

        let tool_calls = map_to_tool_calls(tool_calls_map);
        tracing::info!(
            "LLM reply completed: {} content chars, {} tool calls",
            content.len(),
            tool_calls.len()
        );
        for tc in &tool_calls {
            tracing::info!(
                "Tool call parsed: {} (id: {}) args: {}",
                tc.function.name,
                tc.id,
                tc.function.arguments
            );
        }

        let reply = StreamedReply {
            content,
            reasoning,
            raw,
            tool_calls,
        };
        let out_toks = count_reply_tokens(&reply.content, &reply.reasoning, &reply.tool_calls);
        record_tokens_out(out_toks);
        Ok(reply)
    }
}

fn map_to_tool_calls(
    map: std::collections::BTreeMap<usize, (Option<String>, String, String)>,
) -> Vec<crate::types::ToolCall> {
    map.into_values()
        .map(|(id, name, arguments)| {
            let call_id = id.unwrap_or_else(|| format!("call_{}", uuid::Uuid::new_v4()));
            crate::types::ToolCall::new(call_id, name, arguments)
        })
        .collect()
}

fn consume_event<F>(
    ev: &eventsource_stream::Event,
    content: &mut String,
    reasoning: &mut String,
    raw: &mut String,
    tool_calls_map: &mut std::collections::BTreeMap<usize, (Option<String>, String, String)>,
    in_reasoning: &mut bool,
    on_delta: &mut F,
) -> Result<bool, ChatError>
where
    F: FnMut(&str) -> bool,
{
    if ev.data.trim() == "[DONE]" {
        if *in_reasoning {
            *in_reasoning = false;
            let _ = on_delta("</think>");
        }
        return Ok(true);
    }
    if let Ok(chunk) = serde_json::from_str::<ChatChunk>(&ev.data) {
        for choice in chunk.choices {
            if let Some(r) = choice.delta.reasoning_content
                && !r.is_empty()
            {
                reasoning.push_str(&r);
                raw.push_str(&r);
                if !*in_reasoning {
                    *in_reasoning = true;
                    if !on_delta("<think>") {
                        return Ok(true);
                    }
                }
                if !on_delta(&r) {
                    return Ok(true);
                }
            }
            if let Some(c) = choice.delta.content
                && !c.is_empty()
            {
                if *in_reasoning {
                    *in_reasoning = false;
                    if !on_delta("</think>") {
                        return Ok(true);
                    }
                }
                content.push_str(&c);
                raw.push_str(&c);
                if !on_delta(&c) {
                    return Ok(true);
                }
            }
            if let Some(tcs) = choice.delta.tool_calls {
                for tc in tcs {
                    let entry = tool_calls_map
                        .entry(tc.index)
                        .or_insert_with(|| (None, String::new(), String::new()));
                    if let Some(id) = tc.id {
                        entry.0 = Some(id);
                    }
                    if let Some(func) = tc.function {
                        if let Some(name) = func.name {
                            entry.1.push_str(&name);
                        }
                        if let Some(args) = func.arguments {
                            entry.2.push_str(&args);
                        }
                    }
                }
            }
        }
    }
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Message;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn sse_body(text: &str) -> String {
        format!(
            "data: {}\n\ndata: [DONE]\n\n",
            serde_json::json!({
                "id": "chatcmpl-1",
                "choices": [{
                    "delta": { "content": text },
                    "finish_reason": null
                }]
            })
        )
    }

    fn request() -> ChatRequest {
        ChatRequest {
            model: String::new(),
            messages: vec![Message::User {
                content: "hi".to_string(),
            }],
            temperature: None,
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stream: None,
            enable_thinking: None,
            tools: None,
        }
    }

    #[tokio::test]
    async fn test_llm_retry_backoff() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;
        let calls = Arc::new(AtomicUsize::new(0));

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with({
                let calls = calls.clone();
                move |_req: &Request| {
                    let n = calls.fetch_add(1, Ordering::SeqCst);
                    match n {
                        0 => ResponseTemplate::new(503),
                        1 => ResponseTemplate::new(429),
                        _ => ResponseTemplate::new(200).set_body_string(sse_body("ok")),
                    }
                }
            })
            .mount(&server)
            .await;

        let client = ChatClient::new(server.uri(), "test-model".to_string());
        let reply = client.chat(&request()).await.unwrap();

        assert_eq!(reply.content, "ok");
        assert_eq!(calls.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn test_llm_retry_exhaustion() {
        use wiremock::matchers::{method, path};
        use wiremock::{Mock, MockServer, Request, ResponseTemplate};

        let server = MockServer::start().await;

        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(move |_req: &Request| ResponseTemplate::new(503))
            .mount(&server)
            .await;

        let client = ChatClient::new(server.uri(), "test-model".to_string());
        let err = client.chat(&request()).await.unwrap_err();
        assert!(err.to_string().contains("503"));
    }
}
