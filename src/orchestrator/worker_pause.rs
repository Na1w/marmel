//! Cooperative, process-local pauses addressed to one live worker registry key.

use std::sync::LazyLock;

use dashmap::DashMap;
use tokio::sync::watch;
use tokio_util::sync::CancellationToken;

static PAUSES: LazyLock<DashMap<String, watch::Sender<bool>>> = LazyLock::new(DashMap::new);

struct PauseDisplay<'a>(&'a str);

impl Drop for PauseDisplay<'_> {
    fn drop(&mut self) {
        super::emit_event(crate::ui::Event::SubagentStreamPause {
            agent_tag: self.0.to_string(),
            paused: false,
        });
    }
}

/// Set one live worker's pause request. The caller must use its exact registry key.
pub fn set_paused(worker_key: &str, paused: bool) -> Result<(), String> {
    super::workers::with_live_worker(worker_key, || {
        let sender = PAUSES
            .entry(worker_key.to_string())
            .or_insert_with(|| watch::channel(false).0);
        sender.send_replace(paused);
    })
    .ok_or_else(|| format!("No live subagent with worker_key '{worker_key}'"))
}

pub(crate) fn is_paused(worker_key: &str) -> bool {
    PAUSES
        .get(worker_key)
        .is_some_and(|sender| *sender.borrow())
}

pub(crate) fn paused_key_for_token(token: &CancellationToken) -> Option<String> {
    super::workers::active_worker_keys_for_token(token)
        .into_iter()
        .find(|key| is_paused(key))
}

pub(crate) fn clear(worker_key: &str) {
    if let Some((_, sender)) = PAUSES.remove(worker_key) {
        sender.send_replace(false);
    }
}

/// Wait at an execution boundary without retaining a pool slot or registry lock.
/// Cancellation always wakes a paused worker; this never cancels any worker itself.
pub(crate) async fn wait(worker_key: &str, token: &CancellationToken) -> bool {
    let receiver = PAUSES.get(worker_key).map(|sender| sender.subscribe());
    let Some(mut receiver) = receiver else {
        return true;
    };
    if !*receiver.borrow() {
        return true;
    }
    super::emit_event(crate::ui::Event::SubagentStreamPause {
        agent_tag: worker_key.to_string(),
        paused: true,
    });
    let _display = PauseDisplay(worker_key);
    while *receiver.borrow_and_update() {
        tokio::select! {
            _ = token.cancelled() => break,
            changed = receiver.changed() => {
                if changed.is_err() {
                    break;
                }
            }
        }
    }
    !token.is_cancelled()
}

pub(crate) fn wait_for_current_worker() -> impl Future<Output = bool> + Send {
    // Capture task-local identity before a synchronous bridge may move the
    // future to another thread/runtime.
    let token = super::CURRENT_WORKER_TOKEN.try_with(Clone::clone).ok();
    async move {
        if let Some(token) = token {
            while let Some(key) = paused_key_for_token(&token) {
                if !wait(&key, &token).await {
                    return false;
                }
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Agent;
    use crate::harness::{ToolCaller, ToolInvocation, dispatch_for_async};
    use crate::llm::{PauseAction, StreamControl, StreamSink};
    use crate::orchestrator::{PreemptibleStreamSink, register_active_worker_with_token};
    use crate::tool_names::{TOOL_PAUSE_SUBAGENT, TOOL_READ_FILE, TOOL_RESUME_SUBAGENT};
    use std::time::Duration;

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
    }

    #[test]
    fn manager_controls_one_worker_and_tool_dispatch_waits_without_cancelling() {
        let _lock = super::super::workers::TEST_WORKERS_MUTEX.lock().unwrap();
        for name in [TOOL_PAUSE_SUBAGENT, TOOL_RESUME_SUBAGENT] {
            assert!(
                crate::types::ToolDef::manager_tools()
                    .iter()
                    .any(|tool| tool.function.name == name)
            );
            assert!(
                !crate::types::ToolDef::default_tools()
                    .iter()
                    .any(|tool| tool.function.name == name)
            );
        }
        runtime().block_on(async {
            let token = CancellationToken::new();
            let guard = register_active_worker_with_token(
                Some("t-880001".into()),
                "coder".into(),
                "task".into(),
                Some(token.clone()),
            );
            let other_token = CancellationToken::new();
            let other = register_active_worker_with_token(
                Some("t-880002".into()),
                "coder".into(),
                "other".into(),
                Some(other_token.clone()),
            );
            let dir = tempfile::tempdir().unwrap();
            let path = dir.path().join("source.txt");
            std::fs::write(&path, "preserved task data").unwrap();
            let read = ToolInvocation {
                name: TOOL_READ_FILE.into(),
                arguments: serde_json::json!({"path": path}),
            };
            let pause = ToolInvocation {
                name: TOOL_PAUSE_SUBAGENT.into(),
                arguments: serde_json::json!({"worker_key": guard.0}),
            };
            for name in [TOOL_PAUSE_SUBAGENT, TOOL_RESUME_SUBAGENT] {
                let invocation = ToolInvocation {
                    name: name.into(),
                    arguments: pause.arguments.clone(),
                };
                let forbidden = ToolCaller::SpecialistWithTools {
                    agent: Agent::Generalist,
                    allowed_tools: vec![name.into()],
                };
                assert!(dispatch_for_async(&invocation, forbidden).await.is_err());
            }
            assert!(
                !dispatch_for_async(&pause, ToolCaller::Manager)
                    .await
                    .unwrap()
                    .is_error
            );
            assert!(is_paused(&guard.0));
            assert!(!is_paused(&other.0));
            assert!(!token.is_cancelled());
            assert!(!other_token.is_cancelled());
            assert!(
                crate::orchestrator::get_active_subtasks_str()
                    .contains("Pause requested (resume_subagent to continue)")
            );

            let future = crate::orchestrator::CURRENT_WORKER_TOKEN.scope(
                token.clone(),
                dispatch_for_async(&read, ToolCaller::Specialist(Agent::Coder)),
            );
            tokio::pin!(future);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut future)
                    .await
                    .is_err()
            );
            assert!(wait(&other.0, &other_token).await);
            // Control calls run as the Manager and therefore do not inherit the worker's gate.
            let resume = ToolInvocation {
                name: TOOL_RESUME_SUBAGENT.into(),
                arguments: pause.arguments,
            };
            assert!(
                !dispatch_for_async(&resume, ToolCaller::Manager)
                    .await
                    .unwrap()
                    .is_error
            );
            let result = tokio::time::timeout(Duration::from_secs(1), &mut future)
                .await
                .unwrap()
                .unwrap();
            assert!(!result.is_error);
            assert!(result.content.contains("preserved task data"));
            assert!(!token.is_cancelled());
        });
    }

    #[test]
    fn active_stream_yields_until_resume_and_cancellation_wakes_a_paused_worker() {
        let _lock = super::super::workers::TEST_WORKERS_MUTEX.lock().unwrap();
        runtime().block_on(async {
            let token = CancellationToken::new();
            let guard = register_active_worker_with_token(
                Some("t-880003".into()),
                "coder".into(),
                "task".into(),
                Some(token.clone()),
            );
            let mut sink = PreemptibleStreamSink::register_full(
                &guard.0,
                Some("coder".into()),
                Some("t-880003".into()),
                Some(token.clone()),
                "model",
            );
            set_paused(&guard.0, true).unwrap();
            assert!(matches!(sink.poll_control(), StreamControl::Pause { .. }));
            let paused = sink.on_pause("");
            tokio::pin!(paused);
            assert!(
                tokio::time::timeout(Duration::from_millis(20), &mut paused)
                    .await
                    .is_err()
            );
            set_paused(&guard.0, false).unwrap();
            assert!(matches!(paused.await, PauseAction::Resume));
            set_paused(&guard.0, true).unwrap();
            token.cancel();
            assert!(
                !tokio::time::timeout(Duration::from_secs(1), wait(&guard.0, &token))
                    .await
                    .unwrap()
            );
        });
    }

    #[test]
    fn invalid_targets_and_completed_workers_cannot_be_paused_or_retain_pause_state() {
        let _lock = super::super::workers::TEST_WORKERS_MUTEX.lock().unwrap();
        for key in [
            "",
            "*",
            "manager",
            "orchestrator",
            "steer_arbitrator",
            "coder",
            "t-880004",
        ] {
            assert!(set_paused(key, true).is_err());
        }
        let token = CancellationToken::new();
        let guard = register_active_worker_with_token(
            Some("t-880004".into()),
            "coder".into(),
            "task".into(),
            Some(token.clone()),
        );
        let nested_guard = register_active_worker_with_token(
            Some("t-880004".into()),
            "coder".into(),
            "same task".into(),
            Some(token),
        );
        let key = guard.0.clone();
        set_paused(&key, true).unwrap();
        drop(guard);
        assert!(is_paused(&key), "a nested registration still owns the task");
        drop(nested_guard);
        assert!(!is_paused(&key));
        assert!(set_paused(&key, false).is_err());
        let replacement = register_active_worker_with_token(
            Some("t-880004".into()),
            "coder".into(),
            "retry".into(),
            Some(CancellationToken::new()),
        );
        assert_eq!(replacement.0, key);
        assert!(!is_paused(&replacement.0));
    }

    #[test]
    fn paused_worker_sends_no_model_request_until_resumed() {
        let _lock = super::super::workers::TEST_WORKERS_MUTEX.lock().unwrap();
        runtime().block_on(async {
            use wiremock::{Mock, MockServer, ResponseTemplate};
            use wiremock::matchers::{method, path};
            let server = MockServer::start().await;
            Mock::given(method("POST")).and(path("/v1/chat/completions"))
                .respond_with(ResponseTemplate::new(200).set_body_string(
                    "data: {\"choices\":[{\"delta\":{\"content\":\"continued\"},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n"
                )).expect(1).mount(&server).await;
            let token = CancellationToken::new();
            let guard = register_active_worker_with_token(Some("t-880005".into()), "coder".into(), "task".into(), Some(token.clone()));
            set_paused(&guard.0, true).unwrap();
            let client = crate::llm::ChatClient::new(format!("{}/v1", server.uri()), "model");
            // An auditor registration must not hide its parent task's pause.
            let _audit = register_active_worker_with_token(Some("t-880005".into()), "validator-coder".into(), "audit".into(), Some(token.clone()));
            let request = crate::types::ChatRequest {
                model: "model".into(), messages: vec![crate::types::Message::User { content: "continue the same task".into() }],
                temperature: None, top_p: None, frequency_penalty: None, presence_penalty: None,
                stream: Some(true), enable_thinking: None, reasoning_effort: None, tools: None,
            };
            let call = crate::orchestrator::CURRENT_WORKER_TOKEN.scope(token.clone(), client.chat_stream(&request, |_| true));
            tokio::pin!(call);
            assert!(tokio::time::timeout(Duration::from_millis(20), &mut call).await.is_err());
            assert!(server.received_requests().await.unwrap().is_empty());
            set_paused(&guard.0, false).unwrap();
            let reply = tokio::time::timeout(Duration::from_secs(2), call).await.unwrap().unwrap();
            assert_eq!(reply.content, "continued");
            assert!(!token.is_cancelled());
        });
    }
}
