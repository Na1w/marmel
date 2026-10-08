//! Model slot coordinator & stream preemption.
//!
//! Enables "slot borrowing" on shared models: when the user issues a mid-flight
//! steering command, active specialist streams using the same model (or default
//! model) are temporarily paused, freeing the GPU/LLM slot for the Steer Arbitrator.
//! Upon arbitration completion, the paused specialist stream is resumed with
//! Assistant Prefill (prefix caching), or aborted if cancelled.

use futures_util::stream::{FuturesUnordered, StreamExt};
use std::collections::HashMap;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, RwLock};
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

use crate::llm::{PauseAction, StreamControl, StreamEvent, StreamSink};
use crate::orchestrator::steer::{
    SteerDecision, SteerSubtaskAction, SteerSubtaskDecision, normalize_steer_subtask_action,
};

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
    /// The stream's **exact routing identity**, resolved through the single
    /// identity model owned by notice routing
    /// ([`crate::orchestrator::notice::worker_routing_identity`]).
    ///
    /// `agent_tag` is the worker's registry tag — `{agent}-{task}`, possibly
    /// carrying the `{base}#{n}` collision handle — so it is treated as the
    /// effective key and split structurally (disambiguator removed, task id
    /// taken from the canonical task-id grammar). The stream's own
    /// `agent_name` / `task_id` are the authoritative fields the sink was
    /// registered with, so they pin the split fields when present.
    fn routing_identity(&self) -> crate::orchestrator::notice::WorkerRoutingIdentity {
        let mut identity = crate::orchestrator::notice::worker_routing_identity(&self.agent_tag);

        if let Some(name) = self.agent_name.as_deref() {
            let name = name.trim();
            if !name.is_empty() {
                identity.agent_name = name.to_ascii_lowercase();
            }
        }

        let authoritative_task = self
            .task_id
            .as_deref()
            .map(|raw| crate::task_id::normalize_task_id_ref(raw).to_ascii_lowercase())
            .filter(|clean| !clean.is_empty());
        identity.task_id = authoritative_task.or(identity.task_id);

        identity
    }

    /// Check if this stream is addressed **exactly** by a target agent name or
    /// tool_call_id.
    ///
    /// Accepted addresses — identical to the notice router
    /// ([`crate::orchestrator::notice::WorkerRoutingIdentity::routes`]): the
    /// broadcast wildcards `*` / `worker`; the exact effective tag
    /// (`researcher-t-001#7`); the exact natural tag (`researcher-t-001`); the
    /// exact agent name or a leading role-family segment of it (`validator` →
    /// `validator-coder`); and the exact task id (`t-001`), decoration- and
    /// case-tolerant through the canonical normalizer.
    ///
    /// t-065: every `contains` / unanchored prefix arm is deliberately gone.
    /// They aborted the wrong stream — a `t-1` cancel killed the `t-10` stream
    /// (`ends_with("-t-1")`/`contains`), a truncated tag such as `coder-t-00`
    /// killed `coder-t-001`, and a bare `coder` killed `validator-coder`. The
    /// argument *composition* is unchanged: either supplied address may identify
    /// the stream, and an absent (empty / decoration-only) address is a
    /// wildcard that matches nothing on its own.
    pub fn matches(&self, target_agent: Option<&str>, target_tool_call_id: &str) -> bool {
        let identity = self.routing_identity();

        let clean_tid =
            crate::task_id::normalize_task_id_ref(target_tool_call_id).to_ascii_lowercase();

        let clean_agent =
            crate::task_id::normalize_task_id_ref(target_agent.unwrap_or("")).to_ascii_lowercase();

        if clean_tid.is_empty() && clean_agent.is_empty() {
            return false;
        }

        (!clean_tid.is_empty() && identity.routes(&clean_tid))
            || (!clean_agent.is_empty() && identity.routes(&clean_agent))
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
        while let Ok(signal) = self.pause_rx.try_recv() {
            // H2: when the coordinator gives up on this stream it drops the
            // acknowledgement receiver and cancels the stream instead. A signal
            // still queued in the channel is therefore stale: dropping it here
            // stops the stream from later pausing, printing "Yielded model
            // slot", and resuming from a pause the orchestrator no longer owns.
            if signal.yielded_tx.is_closed() {
                tracing::debug!(
                    agent_tag = %self.identity.agent_tag,
                    "Discarding stale preemption signal (coordinator already gave up)"
                );
                continue;
            }
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
            // H3: `steer` owns the subtask `action` vocabulary, so every action is
            // normalized once through the single shared normalizer (which rejects
            // and logs unrecognized spellings) instead of being compared raw here.
            let cancel_subtasks: Vec<&SteerSubtaskDecision> = decision
                .map(|d| {
                    d.subtasks
                        .iter()
                        .filter(|st| {
                            normalize_steer_subtask_action(&st.action, &st.tool_call_id)
                                == SteerSubtaskAction::Cancel
                        })
                        .collect()
                })
                .unwrap_or_default();

            for (identity, tx) in list {
                let cancelled = cancel_subtasks
                    .iter()
                    .any(|st| identity.matches(st.agent_name.as_deref(), &st.tool_call_id));
                let action = if cancelled {
                    if let Some(ref token) = identity.cancel_token {
                        token.cancel();
                    }
                    PauseAction::Abort
                } else {
                    // Defined fallback for every non-Cancel action — including a
                    // rejected/unknown action, whose rejection is logged by the
                    // normalizer: keep the stream alive instead of guessing.
                    PauseAction::Resume
                };
                let _ = tx.send(action);
            }
        }
    }
}

/// Upper bound for the whole preemption acknowledgement handshake (H2).
///
/// A preempted stream acknowledges by returning [`StreamControl::Pause`] from
/// `poll_control` and handing its action receiver back from `on_pause`. Every
/// conflicting stream must do so within this window; a stream that never
/// acknowledges is cancelled rather than left running.
///
/// Trade-off: the bound has to be short enough that arbitration is not blocked,
/// so a worker that is blocked mid-tool-call (no SSE chunk to yield, e.g. a long
/// tool execution) is cancelled instead of waited for. It is `pub` so the product
/// layer can raise it if a workload legitimately needs longer to reach a yield
/// point; nothing in this module may wait longer than this for an ack.
pub const PREEMPT_ACK_TIMEOUT: Duration = Duration::from_secs(2);

/// One outstanding preemption acknowledgement, bound to its own stream identity.
///
/// The identity is part of the completion value: acknowledgements are collected
/// concurrently and complete in arbitrary order, so a completion can only be
/// attributed to a stream by carrying the stream identity inside the future
/// (`select_all`/`FuturesUnordered` give no ordering guarantees for the entries
/// they leave behind, so index-parallel vectors desynchronise).
struct PendingAck {
    identity: StreamIdentity,
    rx: oneshot::Receiver<oneshot::Sender<PauseAction>>,
}

impl Future for PendingAck {
    type Output = (
        StreamIdentity,
        Result<oneshot::Sender<PauseAction>, oneshot::error::RecvError>,
    );

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        match Pin::new(&mut this.rx).poll(cx) {
            Poll::Ready(ack) => Poll::Ready((this.identity.clone(), ack)),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Preempt any active specialist stream conflicting with `target_model`.
///
/// Handshake (H2): all pause signals are sent up front and their
/// acknowledgements are then collected concurrently under ONE shared deadline
/// ([`PREEMPT_ACK_TIMEOUT`]). A stream that has not acknowledged when the
/// deadline expires has its own `cancel_token` fired, is excluded from the
/// returned handle, and the call still returns in bounded time — the pause
/// decision can never hang on a stream that cannot yield its model slot, and a
/// non-acking stream is never left running until the turn ends.
///
/// Each acknowledgement travels together with the identity of the stream that
/// produced it, so the handle entries and the cancellations stay attached to the
/// right streams regardless of the order in which the streams acknowledge.
///
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

    // 1. Signal every conflicting stream up front. Waiting for each ack before
    //    signalling the next one delayed arbitration by 2 s per non-acking stream.
    //    Every signal is tracked by a waiter that owns its stream identity, so a
    //    completion can always be attributed to the stream that produced it.
    let mut waiters: FuturesUnordered<PendingAck> = FuturesUnordered::new();
    for (identity, pause_tx) in entries {
        let (yielded_tx, yielded_rx) = oneshot::channel();
        let signal = PauseSignal {
            user_input: user_msg.to_string(),
            yielded_tx,
        };
        if pause_tx.send(signal).is_ok() {
            waiters.push(PendingAck {
                identity,
                rx: yielded_rx,
            });
        }
    }

    // 2. Collect the acknowledgements concurrently under ONE shared deadline.
    //    Completions arrive in arbitrary order and carry their own identity; the
    //    waiters still left in `waiters` when the deadline expires are exactly the
    //    streams that never acknowledged. (Dropping the `next()` future on timeout
    //    does not drop the outstanding waiters, so none of them can be lost here.)
    let mut active_senders = Vec::new();
    let deadline = tokio::time::Instant::now() + PREEMPT_ACK_TIMEOUT;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match tokio::time::timeout(remaining, waiters.next()).await {
            Ok(Some((identity, ack))) => match ack {
                Ok(action_tx) => active_senders.push((identity, action_tx)),
                Err(_) => {
                    // The sink dropped the signal (its stream already finished):
                    // nothing to cancel, and nothing to resume.
                    tracing::debug!(
                        agent_tag = %identity.agent_tag,
                        "Preemption signal dropped by the stream before acknowledgement"
                    );
                }
            },
            // Every conflicting stream either acknowledged or vanished.
            Ok(None) => break,
            // Shared deadline expired: everything still outstanding never
            // acknowledged and is cancelled in step 3.
            Err(_elapsed) => break,
        }
    }

    // 3. H2: cancel every stream that never acknowledged. Previously these
    //    identities were dropped without touching their token, so the worker
    //    stream kept emitting until the whole turn ended.
    for waiter in waiters.iter() {
        let identity = &waiter.identity;
        match identity.cancel_token.as_ref() {
            Some(token) => {
                tracing::warn!(
                    agent_tag = %identity.agent_tag,
                    task_id = identity.task_id.as_deref().unwrap_or(""),
                    ack_timeout_ms = PREEMPT_ACK_TIMEOUT.as_millis() as u64,
                    "Preemption not acknowledged within deadline — cancelling non-acking stream (it may be blocked mid-tool-call and unable to yield its model slot)"
                );
                token.cancel();
            }
            None => {
                tracing::warn!(
                    agent_tag = %identity.agent_tag,
                    task_id = identity.task_id.as_deref().unwrap_or(""),
                    ack_timeout_ms = PREEMPT_ACK_TIMEOUT.as_millis() as u64,
                    "Preemption not acknowledged and stream has no cancellation token — excluded from the preempt handle"
                );
            }
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
        // t-065: a role name that is only a *substring* of another role no
        // longer aborts the wrong stream. `coder` does not address
        // `validator-coder`; the role-family form that does is `validator`.
        assert!(!val_ident.matches(Some("coder"), ""));
        assert!(val_ident.matches(Some("validator"), ""));

        // Negative matches
        assert!(!ident.matches(None, "t-002"));
        assert!(!ident.matches(Some("coder"), "t-002"));
        assert!(!ident.matches(None, ""));
    }

    /// t-065 (cancel path of the preemption handle): the `t-1` / `t-10` pair
    /// must be told apart, and every truncated prefix of a tag or task id must
    /// address nobody.
    #[test]
    fn test_stream_identity_matches_rejects_truncated_prefixes() {
        let short = StreamIdentity {
            agent_tag: "researcher-t-1".to_string(),
            agent_name: Some("researcher".to_string()),
            task_id: Some("t-1".to_string()),
            cancel_token: None,
        };
        let long = StreamIdentity {
            agent_tag: "researcher-t-10".to_string(),
            agent_name: Some("researcher".to_string()),
            task_id: Some("t-10".to_string()),
            cancel_token: None,
        };

        // The exact pair: `t-1` addresses the `t-1` stream and never the
        // `t-10` stream (the legacy arms were `contains` / `ends_with("-{id}")`).
        assert!(short.matches(None, "t-1"));
        assert!(
            !long.matches(None, "t-1"),
            "a Cancel aimed at `t-1` must not abort the `t-10` stream"
        );
        assert!(long.matches(None, "t-10"));
        assert!(!short.matches(None, "t-10"));

        // Whole-tag addressing stays exact.
        assert!(short.matches(None, "researcher-t-1"));
        assert!(
            !long.matches(None, "researcher-t-1"),
            "the `t-10` tag merely contains `t-1`"
        );
        assert!(long.matches(None, "researcher-t-10"));

        // Truncated / extended prefixes of a real tag address nobody.
        for near in [
            "researcher-t",
            "researcher-t-1#",
            "researcher-t-10",
            "researcher-t-100",
            "t-100",
            "t-1#",
            "searcher",
            "earcher-t-1",
        ] {
            assert!(
                !short.matches(None, near),
                "`{near}` is only a fragment of the tag `researcher-t-1`"
            );
            assert!(!short.matches(Some(near), ""));
        }

        // Decoration/case tolerance is unchanged.
        assert!(short.matches(None, "[T-1]"));
        assert!(short.matches(Some("RESEARCHER"), ""));

        // A collision-disambiguated tag is addressed by its effective key or its
        // natural key — never by a sibling handle.
        let disambiguated = StreamIdentity {
            agent_tag: "coder-t-001#7".to_string(),
            agent_name: Some("coder".to_string()),
            task_id: Some("t-001".to_string()),
            cancel_token: None,
        };
        assert!(disambiguated.matches(None, "coder-t-001#7"));
        assert!(disambiguated.matches(None, "coder-t-001"));
        assert!(!disambiguated.matches(None, "coder-t-001#9"));
        assert!(!disambiguated.matches(None, "coder-t-00"));
        assert!(!disambiguated.matches(None, "coder-t-0010"));

        // Empty / decoration-only targets stay "absent", never a wildcard.
        assert!(!disambiguated.matches(None, ""));
        assert!(!disambiguated.matches(Some(""), ""));
        assert!(!disambiguated.matches(None, "[]"));
    }

    /// t-065 kept the legitimate targeting forms: exact agent name, role
    /// family, whole tag, and the deliberate broadcast vocabulary of the router.
    #[test]
    fn test_stream_identity_name_role_family_and_broadcast_targets_still_match() {
        let validator = StreamIdentity {
            agent_tag: "validator-coder-t-002".to_string(),
            agent_name: Some("validator-coder".to_string()),
            task_id: Some("t-002".to_string()),
            cancel_token: None,
        };

        assert!(validator.matches(Some("validator-coder"), ""));
        assert!(validator.matches(Some("validator"), ""));
        assert!(validator.matches(Some("validator-coder-t-002"), ""));
        assert!(validator.matches(None, "validator-coder-t-002"));
        assert!(validator.matches(None, "t-002"));

        // Deliberate broadcast, i.e. the router's broadcast vocabulary.
        assert!(validator.matches(Some("*"), ""));
        assert!(validator.matches(None, "worker"));

        // A role name that is merely a substring of the real role, or a role
        // name belonging to a *different* worker, still addresses nothing.
        assert!(!validator.matches(Some("coder"), ""));
        assert!(!validator.matches(Some("coder-t-002"), ""));
        assert!(!validator.matches(Some("validator-coder-t-00"), ""));
        assert!(!validator.matches(Some("coder-t-0"), ""));
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

    // ---- H2: bounded acknowledgement, non-acking streams get cancelled ----

    /// A preemption stream that never acknowledges must be cancelled, not left
    /// running until the turn ends (H2).
    #[tokio::test]
    async fn test_non_acking_preemption_stream_is_cancelled_within_bound() {
        let cancel_token = tokio_util::sync::CancellationToken::new();
        // Registered on the model but never polls control and never calls
        // `on_pause`: it cannot acknowledge the pause request.
        let _sink = PreemptibleStreamSink::register_full(
            "coder-t-901",
            Some("coder".to_string()),
            Some("t-901".to_string()),
            Some(cancel_token.clone()),
            "model-h2-nonacking",
        );

        let started = std::time::Instant::now();
        let handle = preempt_conflicting_stream("model-h2-nonacking", "steer now").await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < PREEMPT_ACK_TIMEOUT + Duration::from_millis(500),
            "the handshake must be bounded by the shared ack deadline, took {elapsed:?}"
        );
        assert!(
            matches!(handle, PreemptHandle::None),
            "a stream that never acknowledged must not be part of the preempt handle"
        );
        assert!(
            cancel_token.is_cancelled(),
            "H2: the non-acking stream's cancellation token must be fired by the round lifecycle"
        );
    }

    /// Every conflicting stream is awaited under ONE shared deadline (H2), so N
    /// non-acking streams cannot delay arbitration by N x the ack timeout, and a
    /// stream that does acknowledge late is still part of the handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_non_acking_streams_share_one_ack_deadline() {
        let model = "model-h2-parallel";
        let slow_token = tokio_util::sync::CancellationToken::new();
        let mut slow_sink = PreemptibleStreamSink::register_full(
            "coder-t-910",
            Some("coder".to_string()),
            Some("t-910".to_string()),
            Some(slow_token.clone()),
            model,
        );
        let tokens: Vec<tokio_util::sync::CancellationToken> = (0..2)
            .map(|_| tokio_util::sync::CancellationToken::new())
            .collect();
        let _non_ackers: Vec<PreemptibleStreamSink> = tokens
            .iter()
            .enumerate()
            .map(|(idx, token)| {
                PreemptibleStreamSink::register_full(
                    format!("coder-t-91{idx}"),
                    Some("coder".to_string()),
                    Some(format!("t-91{idx}")),
                    Some(token.clone()),
                    model,
                )
            })
            .collect();

        // The one stream that can yield acknowledges late (mid-tool-call style).
        let ack_task = tokio::spawn(async move {
            loop {
                if let StreamControl::Pause { .. } = slow_sink.poll_control() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            tokio::time::sleep(Duration::from_millis(1200)).await;
            slow_sink.on_pause("steer now").await
        });

        let started = std::time::Instant::now();
        let handle = preempt_conflicting_stream(model, "steer now").await;
        let elapsed = started.elapsed();

        assert!(
            elapsed < PREEMPT_ACK_TIMEOUT * 2,
            "H2: 3 streams must share one ack deadline (<= {:?}), took {elapsed:?}",
            PREEMPT_ACK_TIMEOUT * 2
        );
        let PreemptHandle::Active(list) = handle else {
            panic!("H2: the acknowledging stream must be held by the preempt handle");
        };
        assert_eq!(
            list.len(),
            1,
            "H2: only the acknowledging stream may be held by the handle"
        );
        for (_identity, tx) in list {
            let _ = tx.send(PauseAction::Resume);
        }
        assert_eq!(
            ack_task.await.unwrap(),
            PauseAction::Resume,
            "H2: the late acknowledging stream must be resumable"
        );
        assert!(
            !slow_token.is_cancelled(),
            "an acknowledging stream must not be cancelled"
        );
        for (idx, token) in tokens.iter().enumerate() {
            assert!(
                token.is_cancelled(),
                "H2: non-acking stream #{idx} was left uncancelled"
            );
        }
    }

    /// The cancelled stream's task must terminate on its own: no leaked stream
    /// task after the preemption round ends.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn test_non_acking_stream_task_does_not_leak() {
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let _sink = PreemptibleStreamSink::register_full(
            "coder-t-902",
            Some("coder".to_string()),
            Some("t-902".to_string()),
            Some(cancel_token.clone()),
            "model-h2-leak",
        );

        // Stand-in for the worker's streaming loop: it keeps emitting until its
        // own cancellation token fires.
        let loop_token = cancel_token.clone();
        let stream_task = tokio::spawn(async move {
            let mut emitted = 0u64;
            loop {
                tokio::select! {
                    _ = loop_token.cancelled() => break,
                    _ = tokio::time::sleep(Duration::from_millis(10)) => emitted += 1,
                }
            }
            emitted
        });

        let handle = preempt_conflicting_stream("model-h2-leak", "steer now").await;
        assert!(matches!(handle, PreemptHandle::None));

        let finished = tokio::time::timeout(
            PREEMPT_ACK_TIMEOUT + Duration::from_millis(500),
            stream_task,
        )
        .await;
        assert!(
            matches!(finished, Ok(Ok(_))),
            "H2: the non-acking stream task must be cancelled, not leak until the turn ends"
        );
    }

    /// A pause signal left queued for a stream that was already given up on must
    /// be dropped sink-side, so the stream neither pauses nor "resumes" from a
    /// handshake the orchestrator no longer owns.
    #[tokio::test]
    async fn test_stale_pause_signal_dropped_after_non_ack_cancel() {
        let cancel_token = tokio_util::sync::CancellationToken::new();
        let mut sink = PreemptibleStreamSink::register_full(
            "coder-t-903",
            Some("coder".to_string()),
            Some("t-903".to_string()),
            Some(cancel_token.clone()),
            "model-h2-stale",
        );

        let handle = preempt_conflicting_stream("model-h2-stale", "steer now").await;
        assert!(matches!(handle, PreemptHandle::None));
        assert!(cancel_token.is_cancelled());

        assert_eq!(
            sink.poll_control(),
            StreamControl::Continue,
            "H2: the abandoned pause signal must be dropped, not replayed as a pause"
        );
    }

    // ---- H3: subtask `action` normalization at the preemption call site ----

    /// Every spelling of a known `Cancel` action must reach the same branch.
    #[tokio::test]
    async fn test_cancel_action_spellings_all_abort() {
        for (idx, spelling) in [
            "Cancel",
            "cancel",
            "CANCEL",
            "  Cancel  ",
            "cancel_task",
            "Cancel Task",
            "cancel-task",
            "abort",
            "terminate_task",
        ]
        .into_iter()
        .enumerate()
        {
            let model = format!("model-h3-cancel-{idx}");
            let cancel_token = tokio_util::sync::CancellationToken::new();
            let mut sink = PreemptibleStreamSink::register_full(
                "coder-t-920",
                Some("coder".to_string()),
                Some("t-920".to_string()),
                Some(cancel_token.clone()),
                model.clone(),
            );

            let handle_task = {
                let model = model.clone();
                tokio::spawn(
                    async move { preempt_conflicting_stream(&model, "cancel coder").await },
                )
            };
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
                "{spelling:?}: handle must be active"
            );

            let decision = SteerDecision {
                decision: "ForwardToWorker".to_string(),
                response: Some(format!("Cancelling coder via {spelling}")),
                tier: None,
                model: None,
                subtasks: vec![SteerSubtaskDecision {
                    tool_call_id: "coder-t-920".to_string(),
                    action: spelling.to_string(),
                    message: None,
                    agent_name: Some("coder".to_string()),
                    prompt: None,
                    sleep_seconds: None,
                }],
                sleep_seconds: None,
            };

            handle.complete_with_subtask_decision(Some(&decision));

            let action = pause_task.await.unwrap();
            assert_eq!(
                action,
                PauseAction::Abort,
                "H3: action {spelling:?} must normalize to Cancel and abort the stream"
            );
            assert!(
                cancel_token.is_cancelled(),
                "H3: action {spelling:?} must cancel the worker token"
            );
        }
    }

    /// An action outside the vocabulary is rejected: it must not be treated as a
    /// Cancel, and the stream keeps running (the documented fallback).
    #[tokio::test]
    async fn test_unknown_subtask_action_does_not_cancel() {
        for (idx, spelling) in ["remove", "not_an_action", "continue", "", "AbortTask?"]
            .into_iter()
            .enumerate()
        {
            let model = format!("model-h3-unknown-{idx}");
            let cancel_token = tokio_util::sync::CancellationToken::new();
            let mut sink = PreemptibleStreamSink::register_full(
                "coder-t-930",
                Some("coder".to_string()),
                Some("t-930".to_string()),
                Some(cancel_token.clone()),
                model.clone(),
            );

            let handle_task = {
                let model = model.clone();
                tokio::spawn(async move { preempt_conflicting_stream(&model, "steer").await })
            };
            loop {
                if let StreamControl::Pause { .. } = sink.poll_control() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
            let pause_task = tokio::spawn(async move { sink.on_pause("steer").await });
            let handle = handle_task.await.unwrap();
            assert!(matches!(handle, PreemptHandle::Active(_)));

            let decision = SteerDecision {
                decision: "ForwardToWorker".to_string(),
                response: Some("unknown action".to_string()),
                tier: None,
                model: None,
                subtasks: vec![SteerSubtaskDecision {
                    tool_call_id: "coder-t-930".to_string(),
                    action: spelling.to_string(),
                    message: None,
                    agent_name: Some("coder".to_string()),
                    prompt: None,
                    sleep_seconds: None,
                }],
                sleep_seconds: None,
            };

            handle.complete_with_subtask_decision(Some(&decision));

            let action = pause_task.await.unwrap();
            assert_eq!(
                action,
                PauseAction::Resume,
                "H3: unrecognized action {spelling:?} must fall back to Resume, not Cancel"
            );
            assert!(
                !cancel_token.is_cancelled(),
                "H3: unrecognized action {spelling:?} must not cancel the worker"
            );
        }
    }

    /// H2 pairing guard: every acknowledgement must be paired with the stream
    /// that produced it, and the cancellation must hit exactly the streams that
    /// never acknowledged.
    ///
    /// `select_all` reports completions in an arbitrary order and reorders the
    /// remaining futures (`swap_remove`), so acknowledgements cannot be tracked
    /// by position against a parallel identity vector. Eight streams are used
    /// because the mis-pairing is only observable once several acknowledgements
    /// arrive in sequence: four of the seven acknowledging streams are Cancel
    /// targets, so any identity/receiver swap changes which sink is aborted and
    /// which token is fired, and the one non-acking stream must be the only
    /// stream excluded from the handle.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn test_each_preemption_ack_is_paired_with_its_own_stream() {
        const NAMES: [&str; 8] = ["pa", "pb", "pc", "pd", "pe", "pf", "pg", "ph"];
        let model = "model-h2-pairing";

        let tokens: Vec<tokio_util::sync::CancellationToken> = (0..NAMES.len())
            .map(|_| tokio_util::sync::CancellationToken::new())
            .collect();

        // Streams 0..7 acknowledge, staggered so the completion order is known;
        // stream 7 never polls control and therefore never acknowledges.
        let mut ack_tasks = Vec::new();
        for label in 0..NAMES.len() - 1 {
            let mut sink = PreemptibleStreamSink::register_full(
                NAMES[label],
                Some(NAMES[label].to_string()),
                Some(format!("t-96{label}")),
                Some(tokens[label].clone()),
                model,
            );
            ack_tasks.push(tokio::spawn(async move {
                tokio::time::sleep(Duration::from_millis(
                    20 + u64::try_from(label).unwrap() * 15,
                ))
                .await;
                loop {
                    if let StreamControl::Pause { .. } = sink.poll_control() {
                        break;
                    }
                    tokio::time::sleep(Duration::from_millis(2)).await;
                }
                sink.on_pause("steer now").await
            }));
        }
        let _non_acker = PreemptibleStreamSink::register_full(
            NAMES[7],
            Some(NAMES[7].to_string()),
            Some("t-967".to_string()),
            Some(tokens[7].clone()),
            model,
        );

        let started = std::time::Instant::now();
        let handle = preempt_conflicting_stream(model, "steer now").await;
        let elapsed = started.elapsed();
        assert!(
            elapsed < PREEMPT_ACK_TIMEOUT + Duration::from_millis(500),
            "the ack handshake must stay bounded, took {elapsed:?}"
        );

        let PreemptHandle::Active(list) = handle else {
            panic!("H2: the acknowledging streams must be held by the preempt handle");
        };
        assert_eq!(
            list.len(),
            NAMES.len() - 1,
            "H2: exactly the acknowledging streams may be held by the handle"
        );
        let handle = PreemptHandle::Active(list);

        let decision = SteerDecision {
            decision: "ForwardToWorker".to_string(),
            response: Some("pairing".to_string()),
            tier: None,
            model: None,
            subtasks: (0..NAMES.len() - 1)
                .map(|label| SteerSubtaskDecision {
                    tool_call_id: format!("t-96{label}"),
                    action: if label < 4 {
                        "Cancel"
                    } else {
                        crate::tool_names::TOOL_REPLY_TO_ARBITRATOR
                    }
                    .to_string(),
                    message: None,
                    agent_name: Some(NAMES[label].to_string()),
                    prompt: None,
                    sleep_seconds: None,
                })
                .collect(),
            sleep_seconds: None,
        };
        handle.complete_with_subtask_decision(Some(&decision));

        for (label, task) in ack_tasks.into_iter().enumerate() {
            let action = task.await.unwrap();
            let expected = if label < 4 {
                PauseAction::Abort
            } else {
                PauseAction::Resume
            };
            assert_eq!(
                action, expected,
                "H2: stream {} must receive {expected:?}; an acknowledgement was paired with another stream's identity",
                NAMES[label]
            );
            assert_eq!(
                tokens[label].is_cancelled(),
                label < 4,
                "H2: token state of stream {} must follow its own decision (expected cancelled={})",
                NAMES[label],
                label < 4
            );
        }
        assert!(
            tokens[7].is_cancelled(),
            "H2: the stream that never acknowledged must be cancelled"
        );
    }
}
