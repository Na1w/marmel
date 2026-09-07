use super::steer::*;
use crate::agents::{Agent, Deliverable};
use crate::harness::HarnessStats;
use crate::llm::ChatClient;
use std::sync::Arc;

fn decision(decision: &str, response: Option<&str>) -> SteerDecision {
    SteerDecision {
        decision: decision.to_string(),
        response: response.map(str::to_string),
        tier: None,
        model: None,
        subtasks: Vec::new(),
        sleep_seconds: None,
    }
}

#[test]
fn test_respond_directly_json() {
    let json = r#"{"decision": "RespondDirectly", "response": "Executing step 2 in the plan."}"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "RespondDirectly");
    assert_eq!(d.response.as_deref(), Some("Executing step 2 in the plan."));
    assert!(d.subtasks.is_empty());
}

#[test]
fn test_abort_immediately_json() {
    let json = r#"{"decision": "AbortImmediately", "response": null}"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "AbortImmediately");
    assert_eq!(d.response, None);
}

#[test]
fn test_queue_and_continue_json() {
    let json = r#"{"decision": "QueueAndContinue", "response": null}"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "QueueAndContinue");
    assert_eq!(d.response, None);
}

#[test]
fn test_queue_and_continue_with_response_json() {
    let json = r#"{"decision": "QueueAndContinue", "response": "Instruktionen har köats för nästa tur medan pågående uppgifter slutförs."}"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "QueueAndContinue");
    assert_eq!(
        d.response.as_deref(),
        Some("Instruktionen har köats för nästa tur medan pågående uppgifter slutförs.")
    );
}

#[test]
fn test_full_caesar_shape_with_subtasks() {
    // The full caesar `SteerDecisionResponse` shape, including tier/model/subtasks.
    let json = r#"{
        "decision": "ForwardToWorker",
        "response": "Forwarding feedback.",
        "tier": "cloud",
        "model": "ollama:Deepseek4Flash",
        "subtasks": [
            {
                "tool_call_id": "call_123",
                "action": "ForwardNotice",
                "message": "use -O3",
                "agent_name": null,
                "prompt": null
            }
        ]
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "ForwardToWorker");
    assert_eq!(d.tier.as_deref(), Some("cloud"));
    assert_eq!(d.model.as_deref(), Some("ollama:Deepseek4Flash"));
    assert_eq!(d.subtasks.len(), 1);
    assert_eq!(d.subtasks[0].tool_call_id, "call_123");
    assert_eq!(d.subtasks[0].action, "ForwardNotice");
    assert_eq!(d.subtasks[0].message.as_deref(), Some("use -O3"));
}

#[test]
fn test_fallback_queues_when_active_subtasks() {
    // Arbitrator unavailable + active subtasks → queue to preserve ongoing jobs.
    let outcome = resolve_steer_outcome(None, true);
    assert!(matches!(outcome, SteerOutcome::QueueInstruction));
}

#[test]
fn test_fallback_steers_when_no_subtasks() {
    // Arbitrator unavailable + no active subtasks → steer immediately.
    let outcome = resolve_steer_outcome(None, false);
    assert!(matches!(outcome, SteerOutcome::SteerImmediately));
}

#[test]
fn test_decided_when_arbitrator_available() {
    let d = decision("RespondDirectly", Some("hello"));
    let outcome = resolve_steer_outcome(Some(d.clone()), true);
    match outcome {
        SteerOutcome::Decided(dec) => assert_eq!(dec.decision, "RespondDirectly"),
        _ => panic!("expected Decided"),
    }
}

#[test]
fn test_three_decision_branches_roundtrip() {
    // Serialize + deserialize each of the three core branches.
    for (decision_name, response) in [
        ("RespondDirectly", Some("reply_text")),
        ("AbortImmediately", None),
        ("QueueAndContinue", None),
    ] {
        let d = decision(decision_name, response);
        let json = serde_json::to_string(&d).unwrap();
        let back: SteerDecision = serde_json::from_str(&json).unwrap();
        assert_eq!(back.decision, decision_name);
        assert_eq!(back.response, response.map(str::to_string));
    }
}

#[test]
fn test_streaming_response_extractor_basic() {
    let mut extractor = StreamingResponseExtractor::new();
    let chunk1 = "{\"decision\": \"RespondDirectly\", \"response\": \"Hello ";
    let (out1, finished1) = extractor.push_chunk(chunk1);
    assert_eq!(out1, "Hello ");
    assert!(!finished1);

    let chunk2 = "there!\\nThis is ";
    let (out2, finished2) = extractor.push_chunk(chunk2);
    assert_eq!(out2, "there!\nThis is ");
    assert!(!finished2);

    let chunk3 = "ready.\", \"subtasks\": []}";
    let (out3, finished3) = extractor.push_chunk(chunk3);
    assert_eq!(out3, "ready.");
    assert!(finished3);

    // After finished, nothing more is emitted
    let (out4, finished4) = extractor.push_chunk(" extra stuff");
    assert_eq!(out4, "");
    assert!(!finished4);
}

#[test]
fn test_streaming_response_extractor_no_response_field() {
    let mut extractor = StreamingResponseExtractor::new();
    let chunk = "{\"decision\": \"AbortImmediately\", \"subtasks\": []}";
    let (out, finished) = extractor.push_chunk(chunk);
    assert_eq!(out, "");
    assert!(!finished);
    assert!(!extractor.in_response_field);
}

#[test]
fn test_streaming_response_extractor_escapes() {
    let mut extractor = StreamingResponseExtractor::new();
    let chunk = "{\"decision\": \"RespondDirectly\", \"response\": \"\\\"Citat\\\" och \\\\backslashes\\\\ samt \\t tabb och \\u0041\"}";
    let (out, finished) = extractor.push_chunk(chunk);
    assert_eq!(out, "\"Citat\" och \\backslashes\\ samt \t tabb och A");
    assert!(finished);
}

#[test]
fn test_streaming_response_extractor_null_response_field() {
    let mut extractor = StreamingResponseExtractor::new();
    let chunk =
        "{\"decision\": \"QueueAndContinue\", \"response\": null, \"subtasks\": [\"task1\"]}";
    let (out, _finished) = extractor.push_chunk(chunk);
    assert_eq!(out, "");
    assert!(!extractor.in_response_field);
}

#[test]
fn test_extract_tasks_to_delegate_explicit_subtasks() {
    let json = r#"{
        "decision": "DelegateTask",
        "response": "I will run the tests and check files.",
        "subtasks": [
            {
                "tool_call_id": "steer-task-1",
                "action": "DelegateTask",
                "agent_name": "coder",
                "prompt": "Run cargo test"
            },
            {
                "tool_call_id": "steer-task-2",
                "action": "DelegateTask",
                "agent_name": "researcher",
                "prompt": "Search codebase for foo"
            }
        ]
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    let tasks = extract_tasks_to_delegate(&d, "check things");
    assert_eq!(tasks.len(), 2);
    assert_eq!(tasks[0].0, Agent::Coder);
    assert_eq!(tasks[0].1, "steer-task-1");
    assert_eq!(tasks[0].2, "Run cargo test");
    assert_eq!(tasks[1].0, Agent::Researcher);
    assert_eq!(tasks[1].1, "steer-task-2");
    assert_eq!(tasks[1].2, "Search codebase for foo");
}

#[test]
fn test_extract_tasks_to_delegate_toplevel_fallback() {
    let json = r#"{
        "decision": "DelegateTask",
        "response": "Starting coder task directly.",
        "subtasks": []
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    let tasks = extract_tasks_to_delegate(&d, "run cargo check");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].0, Agent::Coder);
    assert_eq!(tasks[0].1, "steer-task-1");
    assert_eq!(tasks[0].2, "run cargo check");
}

#[test]
fn test_extract_tasks_to_delegate_no_tasks() {
    let json = r#"{
        "decision": "RespondDirectly",
        "response": "Currently on step 1.",
        "subtasks": []
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    let tasks = extract_tasks_to_delegate(&d, "status?");
    assert!(tasks.is_empty());
}

#[tokio::test]
async fn test_execute_steer_subtask_runs_specialist() {
    let client = ChatClient::new_with_token("http://127.0.0.1:11434", "mock", "tok");
    let stats = Arc::new(HarnessStats::new());
    let res = execute_steer_subtask(
        &client,
        stats,
        Agent::Coder,
        Some("steer-test-1".to_string()),
        "Inspect git status",
    )
    .await;
    assert!(res.is_ok());
    let d = res.unwrap();
    assert!(matches!(
        d.marker,
        crate::agents::MissionMarker::Complete { .. }
    ));
    assert_eq!(d.task_id.as_deref(), Some("steer-test-1"));
    assert!(d.content.contains("Inspect git status"));
}

#[tokio::test]
async fn test_synthesize_steer_subtask_response_streams_answer() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(
                "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"De 3 testerna som misslyckas är i modul foo.\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
            ),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock", "tok");
    let stats = HarnessStats::new();
    let deliverable = Deliverable {
        marker: crate::agents::MissionMarker::Complete {
            task_id: Some("steer-test-1".to_string()),
        },
        content: "Found 3 failing tests in module foo".to_string(),
        task_id: Some("steer-test-1".to_string()),
    };
    let deliverables = vec![(Agent::Researcher, "steer-test-1".to_string(), deliverable)];
    let mut streamed = Vec::new();
    let res = synthesize_steer_subtask_response(
        &client,
        &stats,
        "Vilka tester misslyckas?",
        &deliverables,
        |delta| streamed.push(delta.to_string()),
    )
    .await;
    assert!(res.is_ok());
    let final_text = res.unwrap();
    assert_eq!(final_text, "De 3 testerna som misslyckas är i modul foo.");
    assert_eq!(
        streamed.join(""),
        "De 3 testerna som misslyckas är i modul foo."
    );
}

#[test]
fn test_sleep_decision_json() {
    let json = r#"{
        "decision": "Sleep",
        "response": "Väntar 10 sekunder...",
        "sleep_seconds": 10
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "Sleep");
    assert_eq!(d.response.as_deref(), Some("Väntar 10 sekunder..."));
    assert_eq!(d.sleep_seconds, Some(10));
}

#[test]
fn test_subtask_sleep_json() {
    let json = r#"{
        "decision": "ForwardToWorker",
        "response": "Beordrar codern att vila",
        "subtasks": [
            {
                "tool_call_id": "coder-1",
                "action": "Sleep",
                "sleep_seconds": 15
            }
        ]
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.subtasks.len(), 1);
    assert_eq!(d.subtasks[0].action, "Sleep");
    assert_eq!(d.subtasks[0].sleep_seconds, Some(15));
}

#[test]
fn test_format_steering_history() {
    assert_eq!(format_steering_history(&[]), "None");

    let history = vec![
        (
            "Vad gör den nu?".to_string(),
            "Codern kör just nu testerna.".to_string(),
        ),
        (
            "Hur många tester är det?".to_string(),
            "Totalt körs 12 tester.".to_string(),
        ),
    ];
    let formatted = format_steering_history(&history);
    assert!(formatted.contains("User: \"Vad gör den nu?\""));
    assert!(formatted.contains("Arbitrator: \"Codern kör just nu testerna.\""));
    assert!(formatted.contains("User: \"Hur många tester är det?\""));
    assert!(formatted.contains("Arbitrator: \"Totalt körs 12 tester.\""));
}

#[tokio::test]
async fn test_arbitrate_steer_context_stream_parses_sleep_tool_call() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // Mock SSE response returning a sleep tool call instead of raw JSON
    let tool_call_chunk = "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_sleep_1\",\"type\":\"function\",\"function\":{\"name\":\"sleep\",\"arguments\":\"{\\\"seconds\\\": 7, \\\"reason\\\": \\\"wait for build\\\"}\"}}]},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n";
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(tool_call_chunk))
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock", "tok");
    let stats = HarnessStats::new();
    let ctx = SteerContext {
        main_goal: "test goal",
        orchestrator_status: "Active",
        pending_approval: "None",
        plan_progress: "None",
        plan_content: "None",
        available_agents: "",
        steering_history: "None",
        user_message: "vänta 7 sekunder",
        active_subtasks: "None",
    };

    let decision = arbitrate_steer_context_stream(&client, &stats, ctx, |_| {}).await;
    assert!(decision.is_some());
    let d = decision.unwrap();
    assert_eq!(d.decision, "Sleep");
    assert_eq!(d.sleep_seconds, Some(7));
    assert!(d.response.as_ref().unwrap().contains("7"));
}
