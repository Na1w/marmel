//! Explicit integration test verifying the specialist validation and revision loop.
//!
//! Workflow under test:
//! 1. Specialist (coder) runs initial turn, writes file `src/lib.rs`, and finishes first draft with `MISSION COMPLETE`.
//! 2. Automated Validator inspects deliverable and rejects (`leave_verdict(verdict="REJECTED", comments="Missing unit tests")`).
//! 3. Revision loop activates: specialist receives critique in its context, calls `write_file` for `tests/lib_test.rs`,
//!    and concludes revision with `MISSION COMPLETE`.
//! 4. Automated Validator re-evaluates and approves (`leave_verdict(verdict="APPROVED")`).
//! 5. Final deliverable is approved and both files exist on disk.

use marmennill::agents::{Agent, IsolatedContext, run_specialist_live};
use marmennill::config::Config;
use marmennill::llm::ChatClient;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn tool_call_sse(call_id: &str, tool_name: &str, args_json: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": call_id,
                        "type": "function",
                        "function": {
                            "name": tool_name,
                            "arguments": args_json
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

fn text_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-test",
            "choices": [{
                "delta": {
                    "content": text
                },
                "finish_reason": "stop"
            }]
        })
    )
}

#[tokio::test]
async fn test_specialist_validation_rejection_and_revision_loop() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let server = MockServer::start().await;
        let call_counter = Arc::new(AtomicUsize::new(0));

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let counter = call_counter.clone();
                move |_req: &wiremock::Request| {
                    let call_idx = counter.fetch_add(1, Ordering::SeqCst);
                    let body = match call_idx {
                        // Turn 0: Specialist creates src/lib.rs
                        0 => {
                            let args = serde_json::json!({
                                "path": "src/lib.rs",
                                "content": "pub fn calculate() -> i32 { 42 }"
                            }).to_string();
                            tool_call_sse("call_write_1", "write_file", &args)
                        }
                        // Turn 1: Specialist concludes initial work
                        1 => text_sse("Initial code written.\n\nMISSION COMPLETE (task-t-001)"),
                        // Turn 2: Validator pass 1 rejects with critique
                        2 => {
                            let args = serde_json::json!({
                                "verdict": "REJECTED",
                                "comments": "Function calculate() is missing unit tests. Please add tests."
                            }).to_string();
                            tool_call_sse("call_val_1", "leave_verdict", &args)
                        }
                        // Turn 3: Specialist revision step 1 creates tests/lib_test.rs
                        3 => {
                            let args = serde_json::json!({
                                "path": "tests/lib_test.rs",
                                "content": "#[test] fn test_calc() { assert_eq!(42, 42); }"
                            }).to_string();
                            tool_call_sse("call_write_2", "write_file", &args)
                        }
                        // Turn 4: Specialist revision step 2 concludes
                        4 => text_sse("Added unit tests per validator critique.\n\nMISSION COMPLETE (task-t-001)"),
                        // Turn 5: Validator pass 2 approves
                        5 => {
                            let args = serde_json::json!({
                                "verdict": "APPROVED",
                                "comments": "All verification checks passed and unit tests exist."
                            }).to_string();
                            tool_call_sse("call_val_2", "leave_verdict", &args)
                        }
                        _ => text_sse("Unexpected call"),
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let specialist_cfg = marmennill::config::SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec![
                "write_file".to_string(),
                "read_file".to_string(),
                "run_command".to_string(),
            ],
            enable_validator: Some(true),
            max_validator_iterations: Some(3),
            ..Default::default()
        };
        let mut cfg = Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            enable_xml_rescue: true,
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("coder".to_string(), specialist_cfg);

        let client = ChatClient::new(&backend_url, "test-model");
        let ctx = IsolatedContext {
            role_system_prompt: "You are the Coder specialist.".to_string(),
            brief: "Implement calculate() function and verify it.".to_string(),
            snippets: vec![],
            task_id: Some("task-t-001".to_string()),
            image_urls: vec![],
            audio_urls: vec![],
        };
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token)
            .await
            .expect("specialist live run should succeed");

        assert_eq!(
            call_counter.load(Ordering::SeqCst),
            6,
            "Expected 6 LLM interactions across initial run, rejection, revision, and approval"
        );

        // Verify workspace files were created by the specialist in turn 0 and revision turn 3
        assert!(
            tmp_path.join("src/lib.rs").exists(),
            "src/lib.rs should exist"
        );
        assert!(
            tmp_path.join("tests/lib_test.rs").exists(),
            "tests/lib_test.rs should exist"
        );

        let lib_content = std::fs::read_to_string(tmp_path.join("src/lib.rs")).unwrap();
        assert!(lib_content.contains("pub fn calculate() -> i32 { 42 }"));

        let test_content = std::fs::read_to_string(tmp_path.join("tests/lib_test.rs")).unwrap();
        assert!(test_content.contains("#[test] fn test_calc()"));

        // Verify deliverable indicates completion and no failure/rejection
        assert!(result.contains("MISSION COMPLETE (task-t-001)"));
        assert!(!result.contains("FAILED"));
        assert_eq!(
            marmennill::agents::MissionMarker::parse(&result),
            Some(marmennill::agents::MissionMarker::Complete {
                task_id: Some("t-001".to_string())
            })
        );
    }).await;
}

#[tokio::test]
async fn test_specialist_chatter_loop_terminates_fast() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path, async move {
        let server = MockServer::start().await;
        let call_counter = Arc::new(AtomicUsize::new(0));

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let counter = call_counter.clone();
                move |_req: &wiremock::Request| {
                    let call_idx = counter.fetch_add(1, Ordering::SeqCst);
                    // Each turn: model outputs repetitive thoughts and conversational response without tools
                    let body = match call_idx {
                        0 => text_sse("<think>I need to create a basic raytracer in C. First I will structure the code.</think>I'll help you create a basic raytracer in C. Let's start by writing the source code."),
                        1 => text_sse("<think>I need to create a basic raytracer in C. First I will structure the code.</think>I'll assist you create a basic raytracer in C. Let's commence by writing the source code."),
                        2 => text_sse("<think>I need to create a basic raytracer in C. First I will structure the code.</think>I'll help you create a basic raytracer in C. Let's get started."),
                        _ => text_sse("<think>I need to create a basic raytracer in C.</think>Still chatting."),
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let specialist_cfg = marmennill::config::SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec![
                "write_file".to_string(),
                "read_file".to_string(),
                "run_command".to_string(),
            ],
            enable_validator: Some(true),
            max_validator_iterations: Some(5),
            ..Default::default()
        };
        let mut cfg = Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            enable_xml_rescue: true,
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("coder".to_string(), specialist_cfg);

        let client = ChatClient::new(&backend_url, "test-model");
        let ctx = IsolatedContext {
            role_system_prompt: "You are the Coder specialist.".to_string(),
            brief: "Create a basic raytracer in C.".to_string(),
            snippets: vec![],
            task_id: Some("task-t-002".to_string()),
            image_urls: vec![],
            audio_urls: vec![],
        };
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Must terminate within at most 3 calls (initial + nudges/repetition detector)
        // and MUST NOT run 5 validation passes with 25 revision calls each!
        let total_calls = call_counter.load(Ordering::SeqCst);
        assert!(
            total_calls <= 3,
            "Expected chatter loop to terminate within 3 turns, but got {} calls",
            total_calls
        );

        // Deliverable must reflect failure because no tools were executed and no work was done
        assert!(result.contains("FAILED"));
    }).await;
}

#[tokio::test]
async fn test_specialist_recovers_after_repetition_nudge_and_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let server = MockServer::start().await;
        let call_counter = Arc::new(AtomicUsize::new(0));

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let counter = call_counter.clone();
                move |_req: &wiremock::Request| {
                    let call_idx = counter.fetch_add(1, Ordering::SeqCst);
                    let body = match call_idx {
                        // Turn 0: Model gets stuck in repetition / chatter
                        0 => text_sse("<think>I need to create a raytracer. First I will structure the code.</think>I will help you write a raytracer in C. Let's start."),
                        // Turn 1 (after repetition nudge): Model heeds corrective instruction, creates the file!
                        1 => {
                            let args = serde_json::json!({
                                "path": "raytracer.c",
                                "content": "#include <stdio.h>\nint main() { printf(\"P3 100 100 255\\n\"); return 0; }"
                            }).to_string();
                            tool_call_sse("call_write_recovered", "write_file", &args)
                        }
                        // Turn 2: Model concludes with MISSION COMPLETE
                        2 => text_sse("File written and verified.\n\nMISSION COMPLETE (task-t-003)"),
                        // Turn 3: Validator inspects and approves
                        3 => {
                            let args = serde_json::json!({
                                "verdict": "APPROVED",
                                "comments": "Raytracer code created and verified."
                            }).to_string();
                            tool_call_sse("call_val_ok", "leave_verdict", &args)
                        }
                        _ => text_sse("Unexpected call"),
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let specialist_cfg = marmennill::config::SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec![
                "write_file".to_string(),
                "read_file".to_string(),
                "run_command".to_string(),
            ],
            enable_validator: Some(true),
            max_validator_iterations: Some(3),
            ..Default::default()
        };
        let mut cfg = Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            enable_xml_rescue: true,
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("coder".to_string(), specialist_cfg);

        let client = ChatClient::new(&backend_url, "test-model");
        let ctx = IsolatedContext {
            role_system_prompt: "You are the Coder specialist.".to_string(),
            brief: "Create raytracer.c and complete the task.".to_string(),
            snippets: vec![],
            task_id: Some("task-t-003".to_string()),
            image_urls: vec![],
            audio_urls: vec![],
        };
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Verify recovery happened: file was created on disk
        assert!(
            tmp_path.join("raytracer.c").exists(),
            "raytracer.c should exist after recovery"
        );

        // Verify deliverable indicates completion
        assert!(result.contains("MISSION COMPLETE (task-t-003)"));
        assert!(!result.contains("FAILED"));
        assert_eq!(
            marmennill::agents::MissionMarker::parse(&result),
            Some(marmennill::agents::MissionMarker::Complete {
                task_id: Some("t-003".to_string())
            })
        );
    }).await;
}

#[tokio::test]
async fn test_specialist_revision_recovers_after_repetition_nudge_and_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let server = MockServer::start().await;
        let call_counter = Arc::new(AtomicUsize::new(0));

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with({
                let counter = call_counter.clone();
                move |_req: &wiremock::Request| {
                    let call_idx = counter.fetch_add(1, Ordering::SeqCst);
                    let body = match call_idx {
                        // Turn 0: Specialist creates src/main.rs
                        0 => {
                            let args = serde_json::json!({
                                "path": "src/main.rs",
                                "content": "fn main() {}"
                            }).to_string();
                            tool_call_sse("call_write_main", "write_file", &args)
                        }
                        // Turn 1: Specialist concludes initial work
                        1 => text_sse("Initial code created.\n\nMISSION COMPLETE (task-t-004)"),
                        // Turn 2: Validator rejects with critique asking for tests
                        2 => {
                            let args = serde_json::json!({
                                "verdict": "REJECTED",
                                "comments": "Please add unit tests in tests/main_test.rs"
                            }).to_string();
                            tool_call_sse("call_val_reject", "leave_verdict", &args)
                        }
                        // Turn 3: Specialist in revision starts by repeating conversational chatter without tools
                        3 => text_sse("<think>I need to add unit tests per validator critique.</think>I'll assist you by writing unit tests."),
                        // Turn 4: Revision corrective nudge cleans up chatter, specialist invokes write_file for tests
                        4 => {
                            let args = serde_json::json!({
                                "path": "tests/main_test.rs",
                                "content": "#[test] fn test_ok() {}"
                            }).to_string();
                            tool_call_sse("call_write_test", "write_file", &args)
                        }
                        // Turn 5: Specialist concludes revision
                        5 => text_sse("Tests added.\n\nMISSION COMPLETE (task-t-004)"),
                        // Turn 6: Validator pass 2 approves
                        6 => {
                            let args = serde_json::json!({
                                "verdict": "APPROVED",
                                "comments": "Unit tests present and verified."
                            }).to_string();
                            tool_call_sse("call_val_ok2", "leave_verdict", &args)
                        }
                        _ => text_sse("Unexpected call"),
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let specialist_cfg = marmennill::config::SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec![
                "write_file".to_string(),
                "read_file".to_string(),
                "run_command".to_string(),
            ],
            enable_validator: Some(true),
            max_validator_iterations: Some(3),
            ..Default::default()
        };
        let mut cfg = Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            enable_xml_rescue: true,
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("coder".to_string(), specialist_cfg);

        let client = ChatClient::new(&backend_url, "test-model");
        let ctx = IsolatedContext {
            role_system_prompt: "You are the Coder specialist.".to_string(),
            brief: "Create main.rs with tests.".to_string(),
            snippets: vec![],
            task_id: Some("task-t-004".to_string()),
            image_urls: vec![],
            audio_urls: vec![],
        };
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &ctx, &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Verify both files exist
        assert!(tmp_path.join("src/main.rs").exists());
        assert!(tmp_path.join("tests/main_test.rs").exists());

        // Verify deliverable indicates completion
        assert!(result.contains("MISSION COMPLETE (task-t-004)"));
        assert!(!result.contains("FAILED"));
    }).await;
}
