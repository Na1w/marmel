use super::helpers::*;
use super::*;
use crate::config::Config;
use crate::manager::context::ContextEngine;
use crate::orchestrator::OrchestratorManager;
use crate::ui::raw::RawRenderer;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

fn config_for_backend(backend: &str) -> Config {
    Config {
        backend_url: format!("{backend}/v1"),
        system_prompt_path: std::path::PathBuf::from("prompts/system.md"),
        ui_mode: "tui".to_string(),
        ..Config::default()
    }
}

fn completion_sse(text: &str) -> String {
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

#[tokio::test]
async fn test_ui_run_session_raw_single_turn() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |_req: &wiremock::Request| {
                calls.fetch_add(1, Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_string(completion_sse("single reply"))
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let mut renderer = RawRenderer::new();
    run_session(&cfg, &mut renderer, Some("goal".to_string()), None)
        .await
        .expect("run_session should complete without error");

    let backend_calls = calls.load(Ordering::SeqCst);
    assert_eq!(backend_calls, 1);
}

fn tool_call_sse(id: &str, name: &str, arguments: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-1",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": id,
                        "type": "function",
                        "function": {
                            "name": name,
                            "arguments": arguments
                        }
                    }]
                },
                "finish_reason": null
            }]
        })
    )
}

#[tokio::test]
async fn test_ui_run_session_executes_tool_calls() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let calls = Arc::new(AtomicUsize::new(0));

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with({
            let calls = calls.clone();
            move |req: &wiremock::Request| {
                let n = calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        "call-glob-1",
                        "glob",
                        r#"{"pattern": "Cargo.toml"}"#,
                    ))
                } else {
                    let body_str = String::from_utf8_lossy(&req.body);
                    assert!(
                        body_str.contains("Cargo.toml") || body_str.contains("call-glob-1"),
                        "second LLM request must contain tool result payload"
                    );
                    ResponseTemplate::new(200).set_body_string(completion_sse("Analysis complete."))
                }
            }
        })
        .mount(&server)
        .await;

    let cfg = config_for_backend(&server.uri());

    let mut renderer = RawRenderer::new();
    run_session(
        &cfg,
        &mut renderer,
        Some("analysera projektet".to_string()),
        None,
    )
    .await
    .expect("run_session should complete");

    let backend_calls = calls.load(Ordering::SeqCst);
    assert_eq!(backend_calls, 2);
}

struct RecordingRenderer {
    subagents: Vec<SubagentDetail>,
    delegation_events: Vec<DelegationEvent>,
    events: Vec<Event>,
}

impl RecordingRenderer {
    fn new() -> Self {
        Self {
            subagents: Vec::new(),
            delegation_events: Vec::new(),
            events: Vec::new(),
        }
    }
}

impl Renderer for RecordingRenderer {
    fn init(&mut self) -> Result<()> {
        Ok(())
    }
    fn on_event(&mut self, event: &Event) {
        self.events.push(event.clone());
        if let Event::Delegation(de) = event {
            self.delegation_events.push(de.clone());
        }
    }
    fn flush(&mut self) -> Result<()> {
        Ok(())
    }
    fn poll_input(&mut self) -> Option<String> {
        None
    }
    fn read_input(&mut self) -> Option<String> {
        None
    }
    fn request_abort(&mut self) {}
    fn aborted(&self) -> bool {
        false
    }
    fn shutdown(&mut self) {}
    fn set_subagents(&mut self, subagents: Vec<SubagentDetail>) {
        self.subagents = subagents;
    }
}

fn test_manager(dir: &tempfile::TempDir) -> OrchestratorManager {
    OrchestratorManager::new(
        crate::llm::ChatClient::new("http://localhost:9999/v1", "test-model"),
        crate::agent::phase::Plan::at(dir.path()),
        Arc::new(crate::harness::HarnessStats::new()),
    )
}

#[test]
fn drain_delegation_events_folds_lifecycle_into_subagent_list() {
    let tmp = tempfile::tempdir().unwrap();
    let manager = test_manager(&tmp);
    let mut renderer = RecordingRenderer::new();
    let mut subagents = Vec::<SubagentDetail>::new();

    {
        let mut ev = manager.delegation_events.lock().unwrap();
        ev.push(DelegationEvent::Started {
            agent: crate::agents::Agent::Coder,
            task: Some("t-1".to_string()),
        });
        ev.push(DelegationEvent::Completed {
            agent: crate::agents::Agent::Coder,
            task: Some("t-1".to_string()),
        });
    }

    drain_delegation_events(Some(&manager), &mut renderer, &mut subagents);

    assert_eq!(renderer.delegation_events.len(), 2);
    assert_eq!(subagents.len(), 1);
    assert_eq!(subagents[0].name, "coder-t-1");
    assert!(!subagents[0].is_active);
}

#[test]
fn test_is_reset_command_matches_aliases() {
    assert!(is_reset_command("/reset"));
    assert!(is_reset_command("  /reset  "));
    assert!(is_reset_command("/RESET"));
    assert!(is_reset_command("/reset_plan"));
    assert!(is_reset_command("/reset-plan"));
    assert!(is_reset_command("/clear_plan"));
    assert!(is_reset_command("/clear-plan"));
    assert!(is_reset_command("/reset_execution_plan"));
    assert!(!is_reset_command("/q"));
    assert!(!is_reset_command("reset"));
    assert!(!is_reset_command("hello world"));
}

#[test]
fn test_handle_reset_command_clears_plan_and_notifies() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plan = crate::agent::phase::Plan::at(tmp.path());
    plan.create("# Plan\n- [ ] [t-1] test\n").unwrap();
    assert!(plan.exists());

    let mut renderer = RecordingRenderer::new();
    let mut ctx = ContextEngine::new(4096);
    ctx.set_system_prompt("sys".to_string());
    ctx.set_goal("goal".to_string());

    handle_reset_command(&plan, &mut renderer, &mut ctx);

    assert!(!plan.exists());
    assert!(
        renderer
            .events
            .iter()
            .any(|ev| matches!(ev, Event::Message(m) if m.contains("cleared and reset")))
    );
    assert!(
        renderer
            .events
            .iter()
            .any(|ev| matches!(ev, Event::Status(s) if s.contains("reset")))
    );
}
