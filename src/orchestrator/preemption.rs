//! Model slot coordinator & stream preemption.
//!
//! Enables "slot borrowing" on shared models: when the user issues a mid-flight
//! steering command, active specialist streams using the same model (or default
//! model) are temporarily paused, freeing the GPU/LLM slot for the Steer Arbitrator.
//! Upon arbitration completion, the paused specialist stream is resumed with
//! Assistant Prefill (prefix caching), or aborted if cancelled.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::llm::{PauseAction, StreamControl, StreamEvent, StreamSink};
use crate::orchestrator::steer::SteerDecision;

static NEXT_STREAM_ID: AtomicU64 = AtomicU64::new(1);

/// Internal message sent to an active stream to request yielding its model slot.
pub struct PauseSignal {
    pub user_input: String,
    /// Channel to send back the oneshot receiver for the resumption action.
    pub yielded_tx: oneshot::Sender<oneshot::Sender<PauseAction>>,
}

/// Detailed identity metadata for an active specialist or validator stream.
#[derive(Debug, Clone)]
pub struct StreamIdentity {
    pub agent_tag: String,
    pub agent_name: Option<String>,
    pub task_id: Option<String>,
    pub cancel_token: Option<tokio_util::sync::CancellationToken>,
}

impl StreamIdentity {
    /// Check if this stream matches a target agent name or tool_call_id.
    ///
    /// Matches robustly against:
    /// - target_tool_call_id matching full tag (e.g. "researcher-t-001"), task_id (e.g. "t-001"), or agent_name ("researcher").
    /// - target_agent matching agent_name (e.g. "researcher") or full tag.
    /// - Suffix/prefix/substring variations, stripping quotes and brackets, case-insensitive.
    pub fn matches(&self, target_agent: Option<&str>, target_tool_call_id: &str) -> bool {
        let tag_lower = self.agent_tag.trim().to_ascii_lowercase();

        let clean_tid = target_tool_call_id
            .trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'')
            .trim()
            .to_ascii_lowercase();

        let clean_agent = target_agent
            .unwrap_or("")
            .trim_matches(|c| c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\'')
            .trim()
            .to_ascii_lowercase();

        if clean_tid.is_empty() && clean_agent.is_empty() {
            return false;
        }

        // 1. Direct or partial match on full agent_tag (e.g. "researcher-t-001")
        if !clean_tid.is_empty()
            && (tag_lower == clean_tid
                || tag_lower.ends_with(&format!("-{clean_tid}"))
                || tag_lower.starts_with(&format!("{clean_tid}-"))
                || tag_lower.contains(&clean_tid))
        {
            return true;
        }
        if !clean_agent.is_empty()
            && (tag_lower == clean_agent
                || tag_lower.starts_with(&format!("{clean_agent}-"))
                || tag_lower.ends_with(&format!("-{clean_agent}"))
                || tag_lower.contains(&clean_agent))
        {
            return true;
        }

        // 2. Direct or partial match on explicit task_id (e.g. "t-001")
        if let Some(ref tid) = self.task_id {
            let tid_clean = tid
                .trim_matches(|c| {
                    c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
                })
                .trim()
                .to_ascii_lowercase();
            if !clean_tid.is_empty()
                && (tid_clean == clean_tid
                    || clean_tid.contains(&tid_clean)
                    || tid_clean.contains(&clean_tid))
            {
                return true;
            }
            if !clean_agent.is_empty() && tid_clean == clean_agent {
                return true;
            }
        }

        // 3. Direct or partial match on explicit agent_name (e.g. "researcher")
        if let Some(ref name) = self.agent_name {
            let name_clean = name.trim().to_ascii_lowercase();
            if !clean_agent.is_empty()
                && (name_clean == clean_agent
                    || name_clean.contains(&clean_agent)
                    || clean_agent.contains(&name_clean))
            {
                return true;
            }
            if !clean_tid.is_empty() && name_clean == clean_tid {
                return true;
            }
        }

        false
    }
}

struct StreamEntry {
    identity: StreamIdentity,
    model: String,
    pause_tx: mpsc::UnboundedSender<PauseSignal>,
}

static ACTIVE_STREAMS: LazyLock<RwLock<HashMap<u64, StreamEntry>>> =
    LazyLock::new(|| RwLock::new(HashMap::new()));

/// Returns true if two model identifiers conflict (i.e. share the same GPU/endpoint).
pub fn models_conflict(m1: &str, m2: &str) -> bool {
    let m1_clean = m1.trim();
    let m2_clean = m2.trim();
    if m1_clean.is_empty() || m2_clean.is_empty() {
        return true;
    }
    m1_clean.eq_ignore_ascii_case(m2_clean)
}

/// RAII StreamSink for background specialists that listens for preemption requests.
pub struct PreemptibleStreamSink {
    id: u64,
    identity: StreamIdentity,
    model: String,
    pause_rx: mpsc::UnboundedReceiver<PauseSignal>,
    pending_signal: Option<PauseSignal>,
}

impl PreemptibleStreamSink {
    /// Register a specialist stream for preemption coordination while running.
    pub fn register(agent_tag: impl Into<String>, model: impl Into<String>) -> Self {
        Self::register_full(agent_tag, None, None, None, model)
    }

    /// Register a specialist stream with full identity metadata and optional cancellation token.
    pub fn register_full(
        agent_tag: impl Into<String>,
        agent_name: Option<String>,
        task_id: Option<String>,
        cancel_token: Option<tokio_util::sync::CancellationToken>,
        model: impl Into<String>,
    ) -> Self {
        let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
        let agent_tag = agent_tag.into();
        let model = model.into();
        let (pause_tx, pause_rx) = mpsc::unbounded_channel();

        let inferred_agent_name = agent_name.or_else(|| {
            if let Some(idx) = agent_tag.find('-') {
                Some(agent_tag[..idx].to_string())
            } else {
                Some(agent_tag.clone())
            }
        });
        let inferred_task_id = task_id.or_else(|| {
            agent_tag
                .rfind('-')
                .map(|idx| agent_tag[idx + 1..].to_string())
        });

        let identity = StreamIdentity {
            agent_tag: agent_tag.clone(),
            agent_name: inferred_agent_name,
            task_id: inferred_task_id,
            cancel_token,
        };

        if let Ok(mut map) = ACTIVE_STREAMS.write() {
            map.insert(
                id,
                StreamEntry {
                    identity: identity.clone(),
                    model: model.clone(),
                    pause_tx,
                },
            );
        }

        Self {
            id,
            identity,
            model,
            pause_rx,
            pending_signal: None,
        }
    }

    pub fn agent_tag(&self) -> &str {
        &self.identity.agent_tag
    }

    pub fn identity(&self) -> &StreamIdentity {
        &self.identity
    }

    pub fn model(&self) -> &str {
        &self.model
    }
}

impl Drop for PreemptibleStreamSink {
    fn drop(&mut self) {
        if let Ok(mut map) = ACTIVE_STREAMS.write() {
            map.remove(&self.id);
        }
    }
}

#[async_trait::async_trait]
impl StreamSink for PreemptibleStreamSink {
    fn emit(&mut self, event: StreamEvent) {
        match event {
            StreamEvent::Content(text) => {
                crate::orchestrator::emit_event(crate::ui::Event::SubagentMessage {
                    agent_tag: self.identity.agent_tag.clone(),
                    text,
                });
            }
            StreamEvent::Thinking(text) => {
                crate::orchestrator::emit_event(crate::ui::Event::SubagentThinking {
                    agent_tag: self.identity.agent_tag.clone(),
                    text,
                });
            }
            StreamEvent::Status(status) => {
                crate::orchestrator::emit_status(status);
            }
        }
    }

    fn poll_control(&mut self) -> StreamControl {
        if let Ok(signal) = self.pause_rx.try_recv() {
            let input = signal.user_input.clone();
            self.pending_signal = Some(signal);
            return StreamControl::Pause { user_input: input };
        }
        StreamControl::Continue
    }

    async fn on_pause(&mut self, _user_input: &str) -> PauseAction {
        if let Some(signal) = self.pending_signal.take() {
            crate::orchestrator::emit_status(format!(
                "[{}] Yielded model slot ({}) to Steer Arbitrator — stream paused",
                self.identity.agent_tag, self.model
            ));
            let (action_tx, action_rx) = oneshot::channel();
            if signal.yielded_tx.send(action_tx).is_err() {
                return PauseAction::Resume;
            }
            match action_rx.await {
                Ok(action) => {
                    if matches!(action, PauseAction::Resume) {
                        crate::orchestrator::emit_status(format!(
                            "[{}] Model slot reclaimed — resuming stream...",
                            self.identity.agent_tag
                        ));
                    }
                    action
                }
                Err(_) => PauseAction::Resume,
            }
        } else {
            PauseAction::Resume
        }
    }
}

/// Handle representing an in-flight preempted model stream.
pub enum PreemptHandle {
    None,
    Active(Vec<(StreamIdentity, oneshot::Sender<PauseAction>)>),
}

impl PreemptHandle {
    pub fn complete_all(self, action: PauseAction) {
        if let PreemptHandle::Active(list) = self {
            for (identity, tx) in list {
                if matches!(action, PauseAction::Abort)
                    && let Some(ref token) = identity.cancel_token
                {
                    token.cancel();
                }
                let _ = tx.send(action);
            }
        }
    }

    pub fn complete_with_subtask_decision(self, decision: Option<&SteerDecision>) {
        if let PreemptHandle::Active(list) = self {
            for (identity, tx) in list {
                let cancelled = decision
                    .map(|d| {
                        d.subtasks.iter().any(|st| {
                            st.action.eq_ignore_ascii_case("Cancel")
                                && identity.matches(st.agent_name.as_deref(), &st.tool_call_id)
                        })
                    })
                    .unwrap_or(false);
                let action = if cancelled {
                    if let Some(ref token) = identity.cancel_token {
                        token.cancel();
                    }
                    PauseAction::Abort
                } else {
                    PauseAction::Resume
                };
                let _ = tx.send(action);
            }
        }
    }
}

/// Preempt any active specialist stream conflicting with `target_model`.
/// Returns a `PreemptHandle` that must be completed with `PauseAction` after steering.
pub async fn preempt_conflicting_stream(target_model: &str, user_msg: &str) -> PreemptHandle {
    let entries: Vec<(StreamIdentity, mpsc::UnboundedSender<PauseSignal>)> = {
        let Ok(map) = ACTIVE_STREAMS.read() else {
            return PreemptHandle::None;
        };
        map.values()
            .filter(|entry| models_conflict(&entry.model, target_model))
            .map(|entry| (entry.identity.clone(), entry.pause_tx.clone()))
            .collect()
    };

    if entries.is_empty() {
        return PreemptHandle::None;
    }

    let mut active_senders = Vec::new();
    for (identity, pause_tx) in entries {
        let (yielded_tx, yielded_rx) = oneshot::channel();
        let signal = PauseSignal {
            user_input: user_msg.to_string(),
            yielded_tx,
        };
        if pause_tx.send(signal).is_ok()
            && let Ok(Ok(action_tx)) =
                tokio::time::timeout(Duration::from_secs(2), yielded_rx).await
        {
            active_senders.push((identity, action_tx));
        }
    }

    if active_senders.is_empty() {
        PreemptHandle::None
    } else {
        PreemptHandle::Active(active_senders)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_models_conflict() {
        assert!(models_conflict("", "llama3.1-8b-instruct"));
        assert!(models_conflict("llama3.1-8b-instruct", ""));
        assert!(models_conflict(
            "llama3.1-8b-instruct",
            "LLAMA3.1-8B-INSTRUCT"
        ));
        assert!(!models_conflict("llama3.1-8b-instruct", "gpt-4o"));
    }

    #[tokio::test]
    async fn test_preempt_and_resume_flow() {
        let mut sink = PreemptibleStreamSink::register("coder", "model-test-flow");

        // Initially no pause requested
        assert_eq!(sink.poll_control(), StreamControl::Continue);

        // Preempt on matching model
        let handle_task = tokio::spawn(async {
            preempt_conflicting_stream("model-test-flow", "status check").await
        });

        // Loop poll_control until Pause is received
        let user_input = loop {
            if let StreamControl::Pause { user_input } = sink.poll_control() {
                break user_input;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        };
        assert_eq!(user_input, "status check");

        // Subagent calls on_pause in a task
        let pause_task = tokio::spawn(async move { sink.on_pause("status check").await });

        // Preempt handle resolves
        let handle = handle_task.await.unwrap();
        assert!(matches!(handle, PreemptHandle::Active(_)));

        // Arbitrator finishes and resumes
        handle.complete_all(PauseAction::Resume);

        // Subagent resumes
        let action = pause_task.await.unwrap();
        assert_eq!(action, PauseAction::Resume);
    }

    #[tokio::test]
    async fn test_preempt_with_targeted_subtask_cancel() {
        let mut sink = PreemptibleStreamSink::register("coder", "model-test-cancel");

        let handle_task = tokio::spawn(async {
            preempt_conflicting_stream("model-test-cancel", "cancel coder").await
        });

        // Loop poll_control until Pause is received
        loop {
            if let StreamControl::Pause { .. } = sink.poll_control() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let pause_task = tokio::spawn(async move { sink.on_pause("cancel coder").await });

        let handle = handle_task.await.unwrap();
        assert!(
            matches!(handle, PreemptHandle::Active(_)),
            "handle must be active"
        );

        let decision = SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: Some("Cancelling coder".to_string()),
            tier: None,
            model: None,
            subtasks: vec![crate::orchestrator::steer::SteerSubtaskDecision {
                tool_call_id: "coder".to_string(),
                action: "Cancel".to_string(),
                message: None,
                agent_name: Some("coder".to_string()),
                prompt: None,
                sleep_seconds: None,
            }],
            sleep_seconds: None,
        };

        handle.complete_with_subtask_decision(Some(&decision));

        let action = pause_task.await.unwrap();
        assert_eq!(action, PauseAction::Abort);
    }

    #[test]
    fn test_stream_identity_matching() {
        let ident = StreamIdentity {
            agent_tag: "researcher-t-001".to_string(),
            agent_name: Some("researcher".to_string()),
            task_id: Some("t-001".to_string()),
            cancel_token: None,
        };

        // Match by task_id
        assert!(ident.matches(None, "t-001"));
        assert!(ident.matches(None, "[t-001]"));
        assert!(ident.matches(None, "\"t-001\""));

        // Match by agent_name
        assert!(ident.matches(Some("researcher"), ""));
        assert!(ident.matches(Some("RESEARCHER"), ""));

        // Match by both
        assert!(ident.matches(Some("researcher"), "t-001"));

        // Match by full tag in tool_call_id
        assert!(ident.matches(None, "researcher-t-001"));

        // Validator tag matching
        let val_ident = StreamIdentity {
            agent_tag: "validator-coder-t-002".to_string(),
            agent_name: Some("validator-coder".to_string()),
            task_id: Some("t-002".to_string()),
            cancel_token: None,
        };
        assert!(val_ident.matches(None, "t-002"));
        assert!(val_ident.matches(Some("validator-coder"), ""));
        assert!(val_ident.matches(Some("coder"), ""));

        // Negative matches
        assert!(!ident.matches(None, "t-002"));
        assert!(!ident.matches(Some("coder"), "t-002"));
        assert!(!ident.matches(None, ""));
    }

    #[tokio::test]
    async fn test_preempt_with_task_id_cancel_and_cancellation_token() {
        let cancel_token = tokio_util::sync::CancellationToken::new();
        assert!(!cancel_token.is_cancelled());

        let mut sink = PreemptibleStreamSink::register_full(
            "researcher-t-001",
            Some("researcher".to_string()),
            Some("t-001".to_string()),
            Some(cancel_token.clone()),
            "model-test-task-cancel",
        );

        let handle_task = tokio::spawn(async {
            preempt_conflicting_stream("model-test-task-cancel", "stalled agent check").await
        });

        // Loop poll_control until Pause is received
        loop {
            if let StreamControl::Pause { .. } = sink.poll_control() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }

        let pause_task = tokio::spawn(async move { sink.on_pause("stalled agent check").await });

        let handle = handle_task.await.unwrap();
        assert!(
            matches!(handle, PreemptHandle::Active(_)),
            "handle must be active"
        );

        // Arbitrator cancels t-001 using just the task_id
        let decision = SteerDecision {
            decision: "DelegateTask".to_string(),
            response: Some(
                "Terminating stalled researcher and delegating investigative agent".to_string(),
            ),
            tier: None,
            model: None,
            subtasks: vec![
                crate::orchestrator::steer::SteerSubtaskDecision {
                    tool_call_id: "t-001".to_string(),
                    action: "Cancel".to_string(),
                    message: None,
                    agent_name: Some("researcher".to_string()),
                    prompt: None,
                    sleep_seconds: None,
                },
                crate::orchestrator::steer::SteerSubtaskDecision {
                    tool_call_id: "steer-task-1".to_string(),
                    action: "DelegateTask".to_string(),
                    message: None,
                    agent_name: Some("coder".to_string()),
                    prompt: Some("Investigate stalled task".to_string()),
                    sleep_seconds: None,
                },
            ],
            sleep_seconds: None,
        };

        handle.complete_with_subtask_decision(Some(&decision));

        let action = pause_task.await.unwrap();
        assert_eq!(action, PauseAction::Abort);
        assert!(
            cancel_token.is_cancelled(),
            "cancellation token must be triggered on Cancel"
        );
    }
}
