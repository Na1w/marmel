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

struct StreamEntry {
    agent_tag: String,
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
    agent_tag: String,
    model: String,
    pause_rx: mpsc::UnboundedReceiver<PauseSignal>,
    pending_signal: Option<PauseSignal>,
}

impl PreemptibleStreamSink {
    /// Register a specialist stream for preemption coordination while running.
    pub fn register(agent_tag: impl Into<String>, model: impl Into<String>) -> Self {
        let id = NEXT_STREAM_ID.fetch_add(1, Ordering::Relaxed);
        let agent_tag = agent_tag.into();
        let model = model.into();
        let (pause_tx, pause_rx) = mpsc::unbounded_channel();

        if let Ok(mut map) = ACTIVE_STREAMS.write() {
            map.insert(
                id,
                StreamEntry {
                    agent_tag: agent_tag.clone(),
                    model: model.clone(),
                    pause_tx,
                },
            );
        }

        Self {
            id,
            agent_tag,
            model,
            pause_rx,
            pending_signal: None,
        }
    }

    pub fn agent_tag(&self) -> &str {
        &self.agent_tag
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
    fn emit(&mut self, _event: StreamEvent) {}

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
                self.agent_tag, self.model
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
                            self.agent_tag
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
    Active(Vec<(String, oneshot::Sender<PauseAction>)>),
}

impl PreemptHandle {
    pub fn complete_all(self, action: PauseAction) {
        if let PreemptHandle::Active(list) = self {
            for (_, tx) in list {
                let _ = tx.send(action);
            }
        }
    }

    pub fn complete_with_subtask_decision(self, decision: Option<&SteerDecision>) {
        if let PreemptHandle::Active(list) = self {
            for (agent_tag, tx) in list {
                let cancelled = decision
                    .map(|d| {
                        d.subtasks.iter().any(|st| {
                            st.action == "Cancel"
                                && (st.agent_name.as_deref() == Some(&agent_tag)
                                    || agent_tag.starts_with(&st.tool_call_id)
                                    || (!st.tool_call_id.is_empty()
                                        && st.tool_call_id == agent_tag))
                        })
                    })
                    .unwrap_or(false);
                let action = if cancelled {
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
    let entries: Vec<(String, mpsc::UnboundedSender<PauseSignal>)> = {
        let Ok(map) = ACTIVE_STREAMS.read() else {
            return PreemptHandle::None;
        };
        map.values()
            .filter(|entry| models_conflict(&entry.model, target_model))
            .map(|entry| (entry.agent_tag.clone(), entry.pause_tx.clone()))
            .collect()
    };

    if entries.is_empty() {
        return PreemptHandle::None;
    }

    let mut active_senders = Vec::new();
    for (agent_tag, pause_tx) in entries {
        let (yielded_tx, yielded_rx) = oneshot::channel();
        let signal = PauseSignal {
            user_input: user_msg.to_string(),
            yielded_tx,
        };
        if pause_tx.send(signal).is_ok()
            && let Ok(Ok(action_tx)) =
                tokio::time::timeout(Duration::from_secs(2), yielded_rx).await
        {
            active_senders.push((agent_tag, action_tx));
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
            }],
        };

        handle.complete_with_subtask_decision(Some(&decision));

        let action = pause_task.await.unwrap();
        assert_eq!(action, PauseAction::Abort);
    }
}
