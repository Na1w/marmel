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
            blueprint: None,
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
        // H6 (hand-off after t-031d): the id bound to a marker must be a
        // boundary-safe `t-…` **token** sitting on the marker's own line. The
        // decoration here carries `task-t-001`, which is one longer token — the
        // deleted unbounded pattern used to cut `t-001` out of it (the exact
        // mis-derivation that checked off work nobody did), so the correct
        // verdict now carries no id, and the caller must pass the bound id
        // explicitly (`check_plan_on_deliverable`'s `task_id` argument).
        assert_eq!(
            marmennill::agents::MissionMarker::parse(&result),
            Some(marmennill::agents::MissionMarker::Complete { task_id: None })
        );
        // Positive control: a boundary-safe spelling of the same id does bind.
        assert_eq!(
            marmennill::agents::MissionMarker::parse("Work done.\n\nMISSION COMPLETE (t-001)"),
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
            blueprint: None,
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
            blueprint: None,
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
        // H6: `task-t-003` is one token; cutting `t-003` out of it was the
        // substring mis-derivation that is now gone (see the guard in
        // `markers::bound_task_id`). Boundary-safe spellings still bind.
        assert_eq!(
            marmennill::agents::MissionMarker::parse(&result),
            Some(marmennill::agents::MissionMarker::Complete { task_id: None })
        );
        assert_eq!(
            marmennill::agents::MissionMarker::parse("Work done.\n\nMISSION COMPLETE (t-003)"),
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
            blueprint: None,
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

// ---------------------------------------------------------------------------
// Gate t-033c — repair of a broken fixture (work item 4).
//
// This fixture used to be called
// `test_specialist_approved_without_explicit_mission_complete` and asserted
// that a specialist reply carrying *no terminal marker* still ended up
// "MISSION COMPLETE" and not "FAILED", because the mock queued "turn 2 = the
// validator approves". It was only ever green because the fixture *was* the
// vulnerability: a marker-less coder reply is never validated — production
// nudges it — so the response staged as "the validator's approval" was in fact
// served to the CODER, which then recorded its own verdict. Self-approval,
// dressed up as a validation loop.
//
// With the t-033b identity gate (`runner/execution.rs::may_record_verdict`
// refuses a verdict from a non-validator) the coder's verdict call is refused,
// the mock falls through to `Unexpected call`, nudge exhaustion ends the run
// with `FAILED (Task incomplete or terminated prematurely)` and the fixture
// went RED. Nothing in production regressed: the fixture lost the only path
// that could certify a deliverable without a validator ever having seen it.
//
// Re-shaped honestly: the coder is served a nudge, then a real terminal reply,
// and it is the VALIDATOR turn that receives the `leave_verdict` APPROVED
// response. The request bodies are asserted so a future regression that again
// routes a verdict to the wrong agent cannot pass silently.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_specialist_without_marker_is_nudged_then_approved_by_validator() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let write_args = serde_json::json!({
            "path": "src/math.rs",
            "content": "pub fn add(a: i32, b: i32) -> i32 { a + b }"
        })
        .to_string();
        let turns = vec![
            // Turn 0: the coder writes the deliverable.
            tool_call_sse("call_write_math", "write_file", &write_args),
            // Turn 1: prose only — no tool call and no terminal marker. Production
            // must nudge this turn, it must never validate it.
            text_sse("I have implemented the add function in src/math.rs with 0 failed tests."),
            // Turn 2: the nudged coder concludes properly.
            text_sse("Implemented add() in src/math.rs.\n\nMISSION COMPLETE (t-005)"),
            // Turn 3: the VALIDATOR turn — the only place a verdict may come from.
            verdict_call("APPROVED", "Function verified and correctly implemented."),
        ];
        let (server, call_counter, bodies) = mock_backend(turns).await;

        let backend_url = format!("{}/v1", server.uri());
        let cfg = coder_cfg_with_validator(&backend_url, 3);
        let client = ChatClient::new(&backend_url, "test-model");
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &coder_ctx("t-005"), &cfg, &token)
            .await
            .expect("specialist live run should complete");

        assert!(tmp_path.join("src/math.rs").exists());

        // Exactly four rounds: coder write, coder prose, coder marker, validator
        // verdict. Anything else means a turn was served to the wrong agent.
        assert_eq!(
            call_counter.load(Ordering::SeqCst),
            4,
            "expected one nudge round for the coder plus one validator pass, got: {result}"
        );

        let requests = bodies.lock().expect("capture lock").clone();

        // Proof 1: the marker-less reply was nudged, not validated.
        assert!(
            requests[2].contains("You did not call any tools"),
            "the marker-less coder turn must be nudged instead of validated, got: {}",
            requests[2]
        );

        // Proof 2: the verdict response was served to the VALIDATOR — that request
        // is the audit brief carrying the deliverable under review.
        assert!(
            requests[3].contains("Specialist Deliverable:"),
            "request 4 must be the validator's audit brief, got: {}",
            requests[3]
        );
        assert!(
            requests[3].contains("I have implemented the add function"),
            "the validator must be auditing the coder's deliverable text, got: {}",
            requests[3]
        );

        // The approval came through the legitimate validator path.
        assert!(
            result.contains("MISSION COMPLETE (t-005)"),
            "an explicitly approved deliverable must complete, got: {result}"
        );
        assert!(!result.contains("FAILED"), "got: {result}");
        assert!(!result.contains("VALIDATOR REJECTION"), "got: {result}");

        let marker = marmennill::agents::MissionMarker::parse(&result);
        assert_eq!(
            marker,
            Some(marmennill::agents::MissionMarker::Complete {
                task_id: Some("t-005".to_string())
            }),
            "Marker must be Complete: {result}"
        );
    })
    .await;
}

// ---------------------------------------------------------------------------
// INTENTIONAL CONTRACT CHANGE (gate t-033c, work item 2).
//
// This test used to be named `test_validator_reminded_3_times_and_assumed_
// approved` and pinned the previous fail-OPEN contract: after three reminders
// and still no `leave_verdict` call, the fix loop ended the run with
// `approved = true` ("assumed approved"). A deliverable was therefore
// certified — and its plan line checked off — by a validator that had recorded
// no verdict at all.
//
// The contract is now deliberately fail-CLOSED: the absence of a verdict is not
// a verdict. The run ends with an explicit failed outcome, the deliverable is
// reported as rejected (VALIDATOR REJECTION + FAILED trailer), its completion
// marker is revoked, and the `- [ ] [t-006]` plan line stays unchecked. The
// rewrite below is not a regression caught by accident: it is the new contract,
// on purpose.
// ---------------------------------------------------------------------------
#[tokio::test]
async fn test_validator_reminded_3_times_and_fails_closed_without_verdict() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let write_args =
            serde_json::json!({"path": "src/util.rs", "content": "pub fn util() {}"}).to_string();
        let turns = vec![
            // Turn 0: the coder creates src/util.rs.
            tool_call_sse("call_write_util", "write_file", &write_args),
            // Turn 1: the coder concludes with a terminal marker → validator pass 1.
            text_sse("src/util.rs written.\n\nMISSION COMPLETE (t-006)"),
            // Turns 2..5: the validator talks in prose and never calls the verdict
            // tool. Three reminders, then the fail-closed exit.
            text_sse("I checked the code and it looks solid."),
            text_sse("Still looks fine, no errors found."),
            text_sse("Verification passes, all good."),
            text_sse("Concluded review in text."),
            // Turn 6: the coder reacts to the rejection feedback and revises.
            text_sse("Revised as requested.\n\nMISSION COMPLETE (t-006)"),
        ];
        let (server, call_counter, bodies) = mock_backend(turns).await;
        let plan = seed_plan(&tmp_path, "t-006");

        let backend_url = format!("{}/v1", server.uri());
        let cfg = coder_cfg_with_validator(&backend_url, 1);
        let client = ChatClient::new(&backend_url, "test-model");
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &coder_ctx("t-006"), &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Coder (2) + validator (4: three reminders and the fail-closed round) +
        // one coder revision. Nothing is silently approved on the side.
        assert_eq!(
            call_counter.load(Ordering::SeqCst),
            7,
            "expected 3 reminders, a fail-closed validator round and one revision, got: {result}"
        );

        let requests = bodies.lock().expect("capture lock").clone();
        // The validator really was the agent being reminded, and its third
        // reminder is visible in the request that produced the last prose turn.
        assert!(
            requests[2].contains("Specialist Deliverable:"),
            "request 3 must be the validator's audit brief"
        );
        assert!(
            requests[5].contains("You have not submitted a verdict"),
            "request 6 must carry the third verdict reminder"
        );
        // The coder is told the deliverable was NOT approved, with the reason.
        assert!(
            requests[6].contains("Validation feedback: The validator tested your changes"),
            "request 7 must feed the fail-closed critique back to the coder"
        );

        // Fail-closed outcome: rejected, explicitly reasoned, and failed.
        assert!(
            result.contains("VALIDATOR REJECTION"),
            "the deliverable must be reported as rejected, got: {result}"
        );
        assert!(
            result.contains("no validation verdict was recorded"),
            "the rejection must state that no verdict was recorded, got: {result}"
        );
        assert!(
            result.contains("FAILED"),
            "a verdict-less validation pass must produce a failed deliverable, got: {result}"
        );
        assert!(
            !result.contains("MISSION COMPLETE"),
            "the completion marker must be revoked, got: {result}"
        );
        assert!(
            !marmennill::agents::MissionMarker::parse(&result).is_some_and(|m| m.is_complete()),
            "a verdict-less run must not parse as a completion, got: {result}"
        );

        // The plan line stays unchecked — the deliverable was never approved.
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some("t-006"), &result)
                .expect("plan check is not an IO error"),
            "the plan line must not be checked off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [ ] [t-006]"),
            "plan line must stay unchecked, got: {plan_text}"
        );
    })
    .await;
}

#[tokio::test]
async fn test_delegated_validator_approved_leave_verdict_stops_loop_immediately() {
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
                        // Turn 0: Validator calls leave_verdict (APPROVED)
                        0 => {
                            let args = serde_json::json!({
                                "verdict": "APPROVED",
                                "comments": "Code inspection complete and verified."
                            })
                            .to_string();
                            tool_call_sse("call_val_ok", "leave_verdict", &args)
                        }
                        // Turn 1 should NEVER be called!
                        _ => text_sse("Should not be called!"),
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let specialist_cfg = marmennill::config::SpecialistConfig {
            backend_url: Some(backend_url.clone()),
            auth_token: Some("test-token".to_string()),
            model: Some("test-model".to_string()),
            enable_validator: Some(false),
            ..Default::default()
        };

        let mut cfg = Config {
            backend_url: backend_url.clone(),
            auth_token: "test-token".to_string(),
            model: "test-model".to_string(),
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("validator".to_string(), specialist_cfg);

        let client = ChatClient::new(&backend_url, "test-model");
        let ctx = IsolatedContext {
            role_system_prompt: "You are the Validator specialist.".to_string(),
            brief: "Audit src/lib.rs.".to_string(),
            snippets: vec![],
            task_id: Some("t-val-01".to_string()),
            image_urls: vec![],
            audio_urls: vec![],
            blueprint: None,
        };
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Validator, &ctx, &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Exactly 1 LLM call must have occurred — loop ended immediately upon leave_verdict!
        assert_eq!(
            call_counter.load(Ordering::SeqCst),
            1,
            "Validator must stop inspection loop immediately on leave_verdict"
        );

        assert!(
            result.contains("MISSION COMPLETE (t-val-01)"),
            "Deliverable should contain MISSION COMPLETE: {result}"
        );
        assert!(
            result.contains("Verdict: APPROVED"),
            "Deliverable should contain approval verdict: {result}"
        );

        let marker = marmennill::agents::MissionMarker::parse(&result);
        assert_eq!(
            marker,
            Some(marmennill::agents::MissionMarker::Complete {
                task_id: Some("t-val-01".to_string())
            }),
            "Marker must be Complete: {result}"
        );
    })
    .await;
}

#[tokio::test]
async fn test_delegated_validator_rejected_leave_verdict_stops_loop_immediately() {
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
                        // Turn 0: Validator calls leave_verdict (REJECTED)
                        0 => {
                            let args = serde_json::json!({
                                "verdict": "REJECTED",
                                "comments": "Missing error handling in parse_args"
                            })
                            .to_string();
                            tool_call_sse("call_val_rej", "leave_verdict", &args)
                        }
                        // Turn 1 should NEVER be called!
                        _ => text_sse("Should not be called!"),
                    };
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let specialist_cfg = marmennill::config::SpecialistConfig {
            backend_url: Some(backend_url.clone()),
            auth_token: Some("test-token".to_string()),
            model: Some("test-model".to_string()),
            enable_validator: Some(false),
            ..Default::default()
        };

        let mut cfg = Config {
            backend_url: backend_url.clone(),
            auth_token: "test-token".to_string(),
            model: "test-model".to_string(),
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("validator".to_string(), specialist_cfg);

        let client = ChatClient::new(&backend_url, "test-model");
        let ctx = IsolatedContext {
            role_system_prompt: "You are the Validator specialist.".to_string(),
            brief: "Audit src/lib.rs.".to_string(),
            snippets: vec![],
            task_id: Some("t-val-02".to_string()),
            image_urls: vec![],
            audio_urls: vec![],
            blueprint: None,
        };
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Validator, &ctx, &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Exactly 1 LLM call must have occurred — loop ended immediately upon leave_verdict!
        assert_eq!(
            call_counter.load(Ordering::SeqCst),
            1,
            "Validator must stop inspection loop immediately on leave_verdict"
        );

        assert!(
            result.contains("FAILED"),
            "Deliverable should contain FAILED: {result}"
        );
        assert!(
            result.contains("Missing error handling in parse_args"),
            "Deliverable should contain critique: {result}"
        );

        let marker = marmennill::agents::MissionMarker::parse(&result);
        assert!(
            matches!(
                marker,
                Some(marmennill::agents::MissionMarker::Failed { .. })
            ),
            "Marker must be Failed: {result}"
        );
    })
    .await;
}

#[tokio::test]
async fn test_delegated_validator_skips_subsequent_tools_in_same_turn_after_leave_verdict() {
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
                    let _ = counter.fetch_add(1, Ordering::SeqCst);
                    // Turn 0: emit leave_verdict followed by read_file in the same turn
                    let body = format!(
                        "data: {}\n\ndata: [DONE]\n\n",
                        serde_json::json!({
                            "id": "chatcmpl-test",
                            "choices": [{
                                "delta": {
                                    "content": null,
                                    "tool_calls": [
                                        {
                                            "index": 0,
                                            "id": "call_v",
                                            "type": "function",
                                            "function": {
                                                "name": "leave_verdict",
                                                "arguments": serde_json::json!({
                                                    "verdict": "APPROVED",
                                                    "comments": "Inspected and clean."
                                                }).to_string()
                                            }
                                        },
                                        {
                                            "index": 1,
                                            "id": "call_rf",
                                            "type": "function",
                                            "function": {
                                                "name": "read_file",
                                                "arguments": serde_json::json!({
                                                    "path": "non_existent_file_should_not_be_read.rs"
                                                }).to_string()
                                            }
                                        }
                                    ]
                                },
                                "finish_reason": "tool_calls"
                            }]
                        })
                    );
                    ResponseTemplate::new(200).set_body_string(body)
                }
            })
            .mount(&server)
            .await;

        let backend_url = format!("{}/v1", server.uri());
        let specialist_cfg = marmennill::config::SpecialistConfig {
            backend_url: Some(backend_url.clone()),
            auth_token: Some("test-token".to_string()),
            model: Some("test-model".to_string()),
            enable_validator: Some(false),
            ..Default::default()
        };

        let mut cfg = Config {
            backend_url: backend_url.clone(),
            auth_token: "test-token".to_string(),
            model: "test-model".to_string(),
            ..Default::default()
        };
        cfg.orchestration
            .specialists
            .insert("validator".to_string(), specialist_cfg);

        let client = ChatClient::new(&backend_url, "test-model");
        let ctx = IsolatedContext {
            role_system_prompt: "You are the Validator specialist.".to_string(),
            brief: "Audit codebase.".to_string(),
            snippets: vec![],
            task_id: Some("t-val-03".to_string()),
            image_urls: vec![],
            audio_urls: vec![],
            blueprint: None,
        };
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Validator, &ctx, &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Verify that leave_verdict concluded immediately with approval
        assert!(result.contains("MISSION COMPLETE (t-val-03)"));
        assert_eq!(call_counter.load(Ordering::SeqCst), 1);
    })
    .await;
}

// ---------------------------------------------------------------------------
// Gate t-033a — a missing / unreadable / blank per-task validation verdict file
// (`.marmel/prompts/{task_id}-validation.md`) must be a HARD validation failure.
// Before the fix the absence of that file *skipped* validation and started the
// run with `validation_passed = true`, so the deliverable was reported as
// validated and its plan line checked off although no validator ever ran
// (H1/H4 in docs/recon_bugs_agents_monitor.md).
// ---------------------------------------------------------------------------

/// Mount a mock backend that replays `turns` in order (falling back to a
/// sentinel reply) and records every request body, so a test can prove which
/// prompt the validator was actually driven by.
async fn mock_backend(
    turns: Vec<String>,
) -> (
    wiremock::MockServer,
    Arc<AtomicUsize>,
    Arc<std::sync::Mutex<Vec<String>>>,
) {
    let server = MockServer::start().await;
    let call_counter = Arc::new(AtomicUsize::new(0));
    let bodies: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let counter = call_counter.clone();
    let captured = bodies.clone();
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            let idx = counter.fetch_add(1, Ordering::SeqCst);
            captured
                .lock()
                .expect("capture lock")
                .push(String::from_utf8_lossy(&req.body).to_string());
            let body = turns
                .get(idx)
                .cloned()
                .unwrap_or_else(|| text_sse("Unexpected call"));
            ResponseTemplate::new(200).set_body_string(body)
        })
        .mount(&server)
        .await;
    (server, call_counter, bodies)
}

/// Coder specialist config with the validator enabled and a bounded number of
/// validation passes.
fn coder_cfg_with_validator(backend_url: &str, max_val_iterations: usize) -> Config {
    let mut cfg = Config {
        backend_url: backend_url.to_string(),
        model: "test-model".to_string(),
        enable_xml_rescue: true,
        ..Default::default()
    };
    cfg.orchestration.specialists.insert(
        "coder".to_string(),
        marmennill::config::SpecialistConfig {
            module: "src/agents/coder.rs".to_string(),
            tools: vec![
                "write_file".to_string(),
                "read_file".to_string(),
                "run_command".to_string(),
            ],
            enable_validator: Some(true),
            max_validator_iterations: Some(max_val_iterations),
            ..Default::default()
        },
    );
    cfg
}

fn coder_ctx(task_id: &str) -> IsolatedContext {
    IsolatedContext {
        role_system_prompt: "You are the Coder specialist.".to_string(),
        brief: format!("Deliver the change for {task_id} and verify it."),
        snippets: vec![],
        task_id: Some(task_id.to_string()),
        image_urls: vec![],
        audio_urls: vec![],
        blueprint: None,
    }
}

/// Seed an active execution plan carrying one unchecked line for `task_id`.
fn seed_plan(root: &std::path::Path, task_id: &str) -> marmennill::manager::Plan {
    let dir = root.join(".marmel");
    std::fs::create_dir_all(&dir).expect("marmel dir");
    std::fs::write(
        dir.join("execution_plan.md"),
        format!("# Execution Plan\n\n### Phase 1: Implementation\n- [ ] [{task_id}] Deliver the change\n"),
    )
    .expect("plan file");
    marmennill::manager::Plan::at(dir)
}

fn prompts_dir_of(root: &std::path::Path) -> std::path::PathBuf {
    root.join(".marmel").join("prompts")
}

/// The specialist's first two turns: write a file, then conclude with a
/// completion marker for `task_id`.
fn specialist_turns(task_id: &str) -> Vec<String> {
    let write_args = serde_json::json!({
        "path": "src/lib.rs",
        "content": "pub fn calculate() -> i32 { 42 }"
    })
    .to_string();
    vec![
        tool_call_sse("call_write_1", "write_file", &write_args),
        text_sse(&format!("Work delivered.\n\nMISSION COMPLETE ({task_id})")),
    ]
}

fn verdict_call(verdict: &str, comments: &str) -> String {
    let args = serde_json::json!({"verdict": verdict, "comments": comments}).to_string();
    tool_call_sse("call_verdict_1", "leave_verdict", &args)
}

#[tokio::test]
async fn test_missing_validation_verdict_file_fails_and_leaves_plan_unchecked() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        // A pre-generated prompt workspace whose verdict file for t-701 is absent.
        std::fs::create_dir_all(prompts_dir_of(&tmp_path)).expect("prompts dir");
        let plan = seed_plan(&tmp_path, "t-701");

        let (server, call_counter, _) = mock_backend(specialist_turns("t-701")).await;
        let backend_url = format!("{}/v1", server.uri());
        let cfg = coder_cfg_with_validator(&backend_url, 3);
        let client = ChatClient::new(&backend_url, "test-model");
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &coder_ctx("t-701"), &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // No validator was consulted: the specialist consumed both turns.
        assert_eq!(
            call_counter.load(Ordering::SeqCst),
            2,
            "no validator pass may be invented when the verdict file is missing"
        );

        // The deliverable is reported as NOT validated, with a reason.
        assert!(
            result.contains("Validation was not performed"),
            "missing verdict file must state the reason, got: {result}"
        );
        assert!(
            result.contains("t-701-validation.md does not exist"),
            "reason must name the missing verdict file, got: {result}"
        );
        assert!(
            result.contains("not validated"),
            "deliverable must be reported as not validated, got: {result}"
        );
        assert!(
            result.contains("FAILED"),
            "a missing verdict file must produce a failed deliverable, got: {result}"
        );
        assert!(
            !result.contains("MISSION COMPLETE (t-701)"),
            "the completion marker must be revoked, got: {result}"
        );

        // The plan line must stay unchecked.
        assert!(
            !marmennill::agents::MissionMarker::parse(&result)
                .is_some_and(|marker| marker.is_complete()),
            "the deliverable must not parse as a completion, got: {result}"
        );
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some("t-701"), &result)
                .expect("plan check is not an IO error"),
            "the plan line must not be checked off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [ ] [t-701]"),
            "plan line must stay unchecked, got: {plan_text}"
        );
    })
    .await;
}

#[tokio::test]
async fn test_present_validation_verdict_with_explicit_approval_still_succeeds() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let prompts = prompts_dir_of(&tmp_path);
        std::fs::create_dir_all(&prompts).expect("prompts dir");
        std::fs::write(
            prompts.join("t-702-validation.md"),
            "# Validation brief AUDIT-BRIEF-702\n\nConfirm calculate() returns 42.\n",
        )
        .expect("verdict file");
        let plan = seed_plan(&tmp_path, "t-702");

        let mut turns = specialist_turns("t-702");
        turns.push(verdict_call(
            "APPROVED",
            "calculate() returns 42 and the unit test exists.",
        ));
        let (server, call_counter, bodies) = mock_backend(turns).await;
        let backend_url = format!("{}/v1", server.uri());
        let cfg = coder_cfg_with_validator(&backend_url, 3);
        let client = ChatClient::new(&backend_url, "test-model");
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &coder_ctx("t-702"), &cfg, &token)
            .await
            .expect("specialist live run should complete");

        // Specialist (2 turns) + one validator pass driven by the recorded brief.
        assert_eq!(call_counter.load(Ordering::SeqCst), 3);
        let requests = bodies.lock().expect("capture lock").clone();
        assert!(
            requests[2].contains("AUDIT-BRIEF-702"),
            "the recorded verdict file must still drive the validator prompt"
        );

        // Success path is unchanged by the fix.
        assert!(
            result.contains("MISSION COMPLETE (t-702)"),
            "an explicitly approved verdict must still succeed, got: {result}"
        );
        assert!(!result.contains("FAILED"), "got: {result}");
        assert!(!result.contains("not validated"), "got: {result}");
        assert_eq!(
            marmennill::agents::MissionMarker::parse(&result),
            Some(marmennill::agents::MissionMarker::Complete {
                task_id: Some("t-702".to_string())
            })
        );
        assert!(
            plan.check_plan_on_deliverable(None, Some("t-702"), &result)
                .expect("plan check is not an IO error"),
            "an approved deliverable must still check the plan line off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [x] [t-702]"),
            "plan line must be checked off, got: {plan_text}"
        );
    })
    .await;
}

#[tokio::test]
async fn test_present_validation_verdict_with_explicit_rejection_fails() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let prompts = prompts_dir_of(&tmp_path);
        std::fs::create_dir_all(&prompts).expect("prompts dir");
        std::fs::write(
            prompts.join("t-703-validation.md"),
            "# Validation brief AUDIT-BRIEF-703\n\nCheck error handling.\n",
        )
        .expect("verdict file");
        let plan = seed_plan(&tmp_path, "t-703");

        let mut turns = specialist_turns("t-703");
        turns.push(verdict_call(
            "REJECTED",
            "CRITIQUE-703: missing error handling in calculate().",
        ));
        turns.push(text_sse(
            "Revised, but no validation pass is left.\n\nMISSION COMPLETE (t-703)",
        ));
        let (server, call_counter, bodies) = mock_backend(turns).await;
        let backend_url = format!("{}/v1", server.uri());
        let cfg = coder_cfg_with_validator(&backend_url, 1);
        let client = ChatClient::new(&backend_url, "test-model");
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &coder_ctx("t-703"), &cfg, &token)
            .await
            .expect("specialist live run should complete");

        assert_eq!(call_counter.load(Ordering::SeqCst), 4);
        let requests = bodies.lock().expect("capture lock").clone();
        assert!(
            requests[2].contains("AUDIT-BRIEF-703"),
            "the recorded verdict file must still drive the validator prompt"
        );

        // The failure comes from the recorded verdict, not from the gap logic.
        assert!(
            result.contains("CRITIQUE-703: missing error handling in calculate()."),
            "the validator critique must be surfaced, got: {result}"
        );
        assert!(!result.contains("not validated"), "got: {result}");
        assert!(result.contains("FAILED"), "got: {result}");
        assert!(
            !result.contains("MISSION COMPLETE (t-703)"),
            "the completion marker must be revoked, got: {result}"
        );
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some("t-703"), &result)
                .expect("plan check is not an IO error"),
            "a rejected deliverable must not check the plan line off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [ ] [t-703]"),
            "plan line must stay unchecked, got: {plan_text}"
        );
    })
    .await;
}

#[tokio::test]
async fn test_unreadable_validation_verdict_file_is_a_hard_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let prompts = prompts_dir_of(&tmp_path);
        std::fs::create_dir_all(&prompts).expect("prompts dir");
        // Garbage that is not valid UTF-8: the file exists but carries no
        // readable verdict.
        std::fs::write(
            prompts.join("t-704-validation.md"),
            [0xFFu8, 0xFE, 0x00, 0x80, 0xC3, 0x28],
        )
        .expect("garbage verdict file");
        let plan = seed_plan(&tmp_path, "t-704");

        let (server, call_counter, _) = mock_backend(specialist_turns("t-704")).await;
        let backend_url = format!("{}/v1", server.uri());
        let cfg = coder_cfg_with_validator(&backend_url, 3);
        let client = ChatClient::new(&backend_url, "test-model");
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &coder_ctx("t-704"), &cfg, &token)
            .await
            .expect("specialist live run should complete");

        assert_eq!(
            call_counter.load(Ordering::SeqCst),
            2,
            "an unreadable verdict file must not be validated by an invented pass"
        );
        assert!(
            result.contains("t-704-validation.md could not be read"),
            "reason must name the unreadable verdict file, got: {result}"
        );
        assert!(result.contains("not validated"), "got: {result}");
        assert!(result.contains("FAILED"), "got: {result}");
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some("t-704"), &result)
                .expect("plan check is not an IO error"),
            "an unreadable verdict file must not check the plan line off"
        );
        let plan_text = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(
            plan_text.contains("- [ ] [t-704]"),
            "plan line must stay unchecked, got: {plan_text}"
        );
    })
    .await;
}

#[tokio::test]
async fn test_blank_validation_verdict_file_is_a_hard_failure() {
    let tmp = tempfile::tempdir().unwrap();
    let tmp_path = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(tmp_path.clone(), async move {
        let prompts = prompts_dir_of(&tmp_path);
        std::fs::create_dir_all(&prompts).expect("prompts dir");
        std::fs::write(prompts.join("t-705-validation.md"), "   \n\t\n").expect("blank file");
        let plan = seed_plan(&tmp_path, "t-705");

        let (server, call_counter, _) = mock_backend(specialist_turns("t-705")).await;
        let backend_url = format!("{}/v1", server.uri());
        let cfg = coder_cfg_with_validator(&backend_url, 3);
        let client = ChatClient::new(&backend_url, "test-model");
        let token = CancellationToken::new();

        let result = run_specialist_live(&client, Agent::Coder, &coder_ctx("t-705"), &cfg, &token)
            .await
            .expect("specialist live run should complete");

        assert_eq!(call_counter.load(Ordering::SeqCst), 2);
        assert!(
            result.contains("t-705-validation.md is empty"),
            "reason must name the blank verdict file, got: {result}"
        );
        assert!(result.contains("not validated"), "got: {result}");
        assert!(result.contains("FAILED"), "got: {result}");
        assert!(
            !plan
                .check_plan_on_deliverable(None, Some("t-705"), &result)
                .expect("plan check is not an IO error"),
            "a blank verdict file must not check the plan line off"
        );
    })
    .await;
}
