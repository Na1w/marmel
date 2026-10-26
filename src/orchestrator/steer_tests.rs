use super::steer::*;
use crate::agents::{Agent, Deliverable};
use crate::harness::HarnessStats;
use crate::llm::ChatClient;
use crate::tool_args::{SLEEP_DEFAULT_SECS, SLEEP_MAX_SECS, SLEEP_MIN_SECS, sleep_duration_secs};
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
    let json = r#"{"decision": "QueueAndContinue", "response": "Instruction queued for next turn while active tasks complete."}"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "QueueAndContinue");
    assert_eq!(
        d.response.as_deref(),
        Some("Instruction queued for next turn while active tasks complete.")
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
    let chunk = "{\"decision\": \"RespondDirectly\", \"response\": \"\\\"Quotes\\\" and \\\\backslashes\\\\ plus \\t tab and \\u0041\"}";
    let (out, finished) = extractor.push_chunk(chunk);
    assert_eq!(out, "\"Quotes\" and \\backslashes\\ plus \t tab and A");
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
fn test_extract_tasks_to_delegate_bracket_stripping() {
    let json = r#"{
        "decision": "DelegateTask",
        "response": "Researching codebase",
        "subtasks": [
            {
                "tool_call_id": "[steer-task-1]",
                "action": "DelegateTask",
                "agent_name": "researcher",
                "prompt": "Find usages"
            }
        ]
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    let tasks = extract_tasks_to_delegate(&d, "check usages");
    assert_eq!(tasks.len(), 1);
    assert_eq!(tasks[0].0, Agent::Researcher);
    assert_eq!(tasks[0].1, "steer-task-1");
    assert_eq!(tasks[0].2, "Find usages");
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

/// The steer seam must record the **ad-hoc no-validation default** where the read side
/// looks for it. Under `cargo test` the live runner short-circuits to a canned deliverable
/// (`agents::runner::try_run_specialist_live` returns `None` because the test exe lives under
/// `/deps/`), so `test_execute_steer_subtask_runs_specialist` proves nothing about validation —
/// this test therefore asserts the durable write that the fix actually adds.
///
/// Two things are pinned: the record lands in `<scoped workspace root>/.marmel/prompts` (the
/// spelling `agents::validation::validation_prompts_dir()` resolves to, i.e. the one the verdict
/// probe reads) and **not** in the CWD-relative `.marmel` that `Plan::default()` is rooted at.
/// A record nobody reads is the same bug with an extra file. The id is unique so the repository's
/// own plan can never collide with it and trigger the plan-collision guard.
#[tokio::test]
async fn test_execute_steer_subtask_authors_the_adhoc_record_for_a_unique_id() {
    let tmp = tempfile::tempdir().unwrap();
    let scoped_root = tmp.path().to_path_buf();
    let prompts = scoped_root.join(".marmel").join("prompts");

    crate::harness::with_workspace_root(scoped_root.clone(), async move {
        let client = ChatClient::new_with_token("http://127.0.0.1:11434", "mock", "tok");
        let stats = Arc::new(HarnessStats::new());
        let res = execute_steer_subtask(
            &client,
            stats,
            Agent::Coder,
            Some("steer-test-adhoc-1".to_string()),
            "Inspect git status",
        )
        .await;
        assert!(
            res.is_ok(),
            "the delegated steer subtask still runs: {res:?}"
        );

        let record = prompts.join("steer-test-adhoc-1-validation-skip.md");
        assert!(
            record.exists(),
            "the steer seam must record the ad-hoc no-validation default under the workspace-root \
             prompts dir the verdict probe reads, expected at {}",
            record.display()
        );
        assert_eq!(
            std::fs::read_to_string(&record).expect("record").trim(),
            crate::agents::prompt_builder::VALIDATION_SKIP_REASON_ADHOC,
            "the record must carry the ad-hoc provenance"
        );
        assert!(
            !crate::agents::prompt_builder::VALIDATION_SKIP_REASON_ADHOC.contains("Plan directive"),
            "an ad-hoc delegation never had a plan directive to cite"
        );
        assert!(
            !prompts.join("steer-test-adhoc-1-validation.md").exists(),
            "the ad-hoc default authors no auditor brief"
        );

        // Nothing was written to the CWD-relative plan dir (`Plan::default()`), which is a
        // different directory from `validation_prompts_dir()` under a scoped root.
        let cwd_prompts = std::path::Path::new(".marmel").join("prompts");
        assert!(
            !cwd_prompts
                .join("steer-test-adhoc-1-validation-skip.md")
                .exists(),
            "the record must not be dropped where the read side never looks: {}",
            cwd_prompts.display()
        );
    })
    .await;
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
                "data: {\"id\":\"chatcmpl-1\",\"choices\":[{\"delta\":{\"content\":\"The 3 failing tests are in module foo.\"},\"finish_reason\":null}]}\n\ndata: [DONE]\n\n",
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
        "Which tests are failing?",
        &deliverables,
        |delta| streamed.push(delta.to_string()),
    )
    .await;
    assert!(res.is_ok());
    let final_text = res.unwrap();
    assert_eq!(final_text, "The 3 failing tests are in module foo.");
    assert_eq!(streamed.join(""), "The 3 failing tests are in module foo.");
}

#[test]
fn test_sleep_decision_json() {
    let json = r#"{
        "decision": "Sleep",
        "response": "Waiting 10 seconds...",
        "sleep_seconds": 10
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(d.decision, "Sleep");
    assert_eq!(d.response.as_deref(), Some("Waiting 10 seconds..."));
    assert_eq!(d.sleep_seconds, Some(10));
}

#[test]
fn test_subtask_sleep_json() {
    let json = r#"{
        "decision": "ForwardToWorker",
        "response": "Ordering coder to sleep",
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
            "What is it doing now?".to_string(),
            "Coder is currently running tests.".to_string(),
        ),
        (
            "How many tests are there?".to_string(),
            "A total of 12 tests are running.".to_string(),
        ),
    ];
    let formatted = format_steering_history(&history);
    assert!(formatted.contains("User: \"What is it doing now?\""));
    assert!(formatted.contains("Arbitrator: \"Coder is currently running tests.\""));
    assert!(formatted.contains("User: \"How many tests are there?\""));
    assert!(formatted.contains("Arbitrator: \"A total of 12 tests are running.\""));
}

#[tokio::test]
async fn test_arbitrate_steer_context_stream_parses_sleep_tool_call() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
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
        user_message: "wait 7 seconds",
        active_subtasks: "None",
    };

    let decision = arbitrate_steer_context_stream(&client, &stats, ctx, |_| {}).await;
    assert!(decision.is_some());
    let d = decision.unwrap();
    assert_eq!(d.decision, "Sleep");
    assert_eq!(d.sleep_seconds, Some(7));
    assert!(d.response.as_ref().unwrap().contains("7"));
}

#[test]
fn test_normalize_steer_decision() {
    assert_eq!(
        normalize_steer_decision(Some("RespondDirectly")),
        "RespondDirectly"
    );
    assert_eq!(
        normalize_steer_decision(Some("respond_directly")),
        "RespondDirectly"
    );
    assert_eq!(
        normalize_steer_decision(Some("Respond Directly")),
        "RespondDirectly"
    );
    assert_eq!(
        normalize_steer_decision(Some("responddirectly")),
        "RespondDirectly"
    );
    assert_eq!(normalize_steer_decision(Some("respond")), "RespondDirectly");
    assert_eq!(normalize_steer_decision(Some("direct")), "RespondDirectly");

    assert_eq!(normalize_steer_decision(Some("Sleep")), "Sleep");
    // The lower-case spelling is the sleep **tool** name itself (the arbitrator
    // asks for the same tool by name), so it is spelled through the shared
    // table instead of as a bare tool-name literal in `src/`.
    assert_eq!(
        normalize_steer_decision(Some(crate::tool_names::TOOL_SLEEP)),
        "Sleep"
    );

    assert_eq!(
        normalize_steer_decision(Some("AbortImmediately")),
        "AbortImmediately"
    );
    assert_eq!(
        normalize_steer_decision(Some("abort_immediately")),
        "AbortImmediately"
    );
    assert_eq!(normalize_steer_decision(Some("abort")), "AbortImmediately");

    assert_eq!(
        normalize_steer_decision(Some("ForwardToWorker")),
        "ForwardToWorker"
    );
    assert_eq!(
        normalize_steer_decision(Some("forward_to_worker")),
        "ForwardToWorker"
    );

    assert_eq!(normalize_steer_decision(Some("ApprovePlan")), "ApprovePlan");
    assert_eq!(
        normalize_steer_decision(Some("approve_plan")),
        "ApprovePlan"
    );

    assert_eq!(normalize_steer_decision(Some("RejectPlan")), "RejectPlan");
    assert_eq!(normalize_steer_decision(Some("reject_plan")), "RejectPlan");

    assert_eq!(
        normalize_steer_decision(Some("DelegateTask")),
        "DelegateTask"
    );
    assert_eq!(
        normalize_steer_decision(Some("delegate_task")),
        "DelegateTask"
    );

    assert_eq!(
        normalize_steer_decision(Some("QueueAndContinue")),
        "QueueAndContinue"
    );
    assert_eq!(
        normalize_steer_decision(Some("queue_and_continue")),
        "QueueAndContinue"
    );

    assert_eq!(normalize_steer_decision(None), "None");
    assert_eq!(
        normalize_steer_decision(Some("completely_unknown")),
        "Unknown"
    );
}

#[tokio::test]
async fn test_arbitrate_steer_context_stream_plain_text_fallback() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let chunk = "data: {\"choices\":[{\"delta\":{\"content\":\"Coder is working on step 1.\"}}]}\n\ndata: [DONE]\n\n";

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(chunk))
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
        user_message: "how is it going?",
        active_subtasks: "None",
    };

    let mut streamed = String::new();
    let decision = arbitrate_steer_context_stream(&client, &stats, ctx, |delta| {
        streamed.push_str(delta);
    })
    .await;

    assert!(decision.is_some());
    let d = decision.unwrap();
    assert_eq!(d.decision, "RespondDirectly");
    assert!(
        d.response
            .as_ref()
            .unwrap()
            .contains("Coder is working on step 1.")
    );
}

// RC-5: one omitted field must never silently drop a forwarded notice.
//
// `SteerSubtaskDecision.tool_call_id` used to have no serde default while every sibling field
// had one, so a perfectly actionable `ForwardNotice` subtask without a handle made the **whole**
// decision unparseable — and an unparseable decision degraded into `RespondDirectly` with no
// notice posted and nothing recorded anywhere.

/// An omitted `tool_call_id` is tolerated: the decision stays a `ForwardToWorker` with its
/// subtask intact, so the notice can still be posted (targeting then decides where).
#[test]
fn test_steer_decision_tolerates_an_omitted_tool_call_id() {
    let json = r#"{"decision":"ForwardToWorker","subtasks":[{"action":"ForwardNotice","agent_name":"coder","message":"rebase onto main"}]}"#;
    let parsed: SteerDecision = serde_json::from_str(json)
        .expect("a missing tool_call_id must not make the whole decision unparseable");

    assert_eq!(
        normalize_steer_decision(Some(&parsed.decision)),
        "ForwardToWorker"
    );
    assert_eq!(parsed.subtasks.len(), 1);
    assert_eq!(parsed.subtasks[0].tool_call_id, "");
    assert_eq!(
        normalize_steer_subtask_action(
            &parsed.subtasks[0].action,
            &parsed.subtasks[0].tool_call_id
        ),
        SteerSubtaskAction::ForwardNotice
    );
    assert_eq!(
        parsed.subtasks[0].message.as_deref(),
        Some("rebase onto main")
    );
}

/// When a decision genuinely cannot be parsed, the fallback stays the **safe** direction (an
/// answer to the user, never a phantom action) but it is **surfaced**: the user is told no
/// specialist was forwarded anything, instead of the steer vanishing.
///
/// The status channel is a process-global registration (`set_status_sender`), so a parallel
/// test can replace it between this test's install and the emit. The round is therefore re-run
/// (bounded) with the sender re-installed each attempt — the assertion itself stays strict.
#[tokio::test]
async fn test_arbitrate_steer_context_stream_surfaces_an_unparseable_decision() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    // A `ForwardNotice` subtask with no `action` at all: unparseable, so no action can run.
    let bad = r#"{"decision":"ForwardToWorker","subtasks":[{"tool_call_id":"call_nope","agent_name":"coder","message":"stop"}]}"#;
    let chunk = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&serde_json::json!({
            "choices": [{ "delta": { "content": bad } }]
        }))
        .expect("sse frame")
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(chunk))
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
        user_message: "tell the coder to stop",
        active_subtasks: "None",
    };

    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut surfaced = false;
    let mut statuses: Vec<String> = Vec::new();

    for _ in 0..5 {
        crate::orchestrator::set_status_sender(status_tx.clone());

        let decision = arbitrate_steer_context_stream(&client, &stats, ctx.clone(), |_| {}).await;

        // Safe direction: a direct answer, no phantom forwarding.
        let d = decision.expect("the raw text still yields a direct answer");
        assert_eq!(
            normalize_steer_decision(Some(&d.decision)),
            "RespondDirectly",
            "an unparseable decision must degrade to a direct answer"
        );
        assert!(
            d.subtasks.is_empty(),
            "no action may run from a bad decision"
        );

        while let Ok(status) = status_rx.try_recv() {
            statuses.push(status);
        }
        if statuses.iter().any(|s| {
            s.contains("unreadable decision") && s.contains("NOT forwarded to any specialist")
        }) {
            surfaced = true;
            break;
        }
    }

    // Observable: the user is told the instruction was not forwarded.
    assert!(
        surfaced,
        "an unreadable decision must be surfaced, got {statuses:?}"
    );
}

/// Source guard: the parse-failure arm of the arbitration must keep surfacing the loss — a
/// `warn!` for the logs **and** a user-visible status naming the fact that nothing was
/// forwarded. Needles are assembled at runtime (style of the sleep guard below) and comment
/// lines are excluded, so this test's own text can never satisfy them.
#[test]
fn unparseable_steer_decision_arms_log_and_surface_the_lost_notice() {
    let src = include_str!("steer.rs");
    let production = src
        .split(&["#[cfg(", "test)", "]"].concat())
        .next()
        .expect("steer.rs always has a first region");
    let code: String = production
        .lines()
        .filter(|line| !line.trim_start().starts_with("//"))
        .collect::<Vec<&str>>()
        .join("\n");

    let arm = concat!("serde_json::from_str::", "<SteerDecision>");
    let region = code
        .split_once(arm)
        .map(|(_head, tail)| {
            tail.split_once("if let Some(decision) = parsed_decision")
                .map_or(tail, |(head, _)| head)
        })
        .expect("the parse of the arbitrator decision must stay a single arm");
    assert!(
        region.contains("tracing::warn"),
        "an unreadable arbitrator decision must be logged, not swallowed"
    );
    assert!(
        region.contains(&["emit_", "status("].concat()),
        "an unreadable arbitrator decision must reach the user through a status line"
    );
    assert!(
        region.contains(&["unreadable deci", "sion"].concat())
            && region.contains(&["forwarded to any s", "pecialist"].concat()),
        "the status must say plainly that no specialist received the instruction"
    );
}

// One shared normalizer for the subtask `action` vocabulary

/// Every spelling of a known action must fold onto the same canonical action,
/// and therefore drive the same branch everywhere it is consumed.
#[test]
fn test_steer_subtask_action_spelling_table() {
    let cases: &[(&str, SteerSubtaskAction)] = &[
        ("Cancel", SteerSubtaskAction::Cancel),
        ("cancel", SteerSubtaskAction::Cancel),
        ("CANCEL", SteerSubtaskAction::Cancel),
        ("  Cancel  ", SteerSubtaskAction::Cancel),
        ("cancel_task", SteerSubtaskAction::Cancel),
        ("Cancel Task", SteerSubtaskAction::Cancel),
        ("cancel-task", SteerSubtaskAction::Cancel),
        ("cancelWorker", SteerSubtaskAction::Cancel),
        ("abort", SteerSubtaskAction::Cancel),
        ("abort_immediately", SteerSubtaskAction::Cancel),
        ("terminate task", SteerSubtaskAction::Cancel),
        ("ForwardNotice", SteerSubtaskAction::ForwardNotice),
        ("forward_notice", SteerSubtaskAction::ForwardNotice),
        ("forward", SteerSubtaskAction::ForwardNotice),
        ("Forward To Worker", SteerSubtaskAction::ForwardNotice),
        ("send notice", SteerSubtaskAction::ForwardNotice),
        (
            crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
            SteerSubtaskAction::ForwardNotice,
        ),
        ("DelegateTask", SteerSubtaskAction::DelegateTask),
        (
            crate::tool_names::TOOL_DELEGATE_TASK,
            SteerSubtaskAction::DelegateTask,
        ),
        ("DELEGATE_TASK", SteerSubtaskAction::DelegateTask),
        ("Delegate Task", SteerSubtaskAction::DelegateTask),
        ("delegate-task", SteerSubtaskAction::DelegateTask),
        ("delegate", SteerSubtaskAction::DelegateTask),
        ("new task", SteerSubtaskAction::DelegateTask),
        ("Sleep", SteerSubtaskAction::Sleep),
        (crate::tool_names::TOOL_SLEEP, SteerSubtaskAction::Sleep),
        ("wait", SteerSubtaskAction::Sleep),
        ("wait_seconds", SteerSubtaskAction::Sleep),
    ];

    for (raw, expected) in cases {
        assert_eq!(
            normalize_steer_subtask_action(raw, "tc-1"),
            *expected,
            "normalize({raw:?})"
        );
        assert_eq!(
            raw.parse::<SteerSubtaskAction>(),
            Ok(*expected),
            "FromStr({raw:?})"
        );
        assert!(
            expected.is_known(),
            "{expected} must be part of the vocabulary"
        );
    }

    for group in [
        &[
            "Cancel",
            "cancel",
            "cancel_task",
            "Cancel Task",
            "cancel-task",
        ][..],
        &[
            "ForwardNotice",
            "forward_notice",
            "forward",
            crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
        ][..],
        &[
            "DelegateTask",
            crate::tool_names::TOOL_DELEGATE_TASK,
            "delegate",
            "DELEGATE TASK",
        ][..],
        &["Sleep", crate::tool_names::TOOL_SLEEP, "wait"][..],
    ] {
        let canonical: Vec<&str> = group
            .iter()
            .map(|raw| normalize_steer_subtask_action(raw, "tc-1").canonical())
            .collect();
        assert_eq!(
            canonical
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len(),
            1,
            "spellings {group:?} must fold onto one canonical action, got {canonical:?}"
        );
    }
}

/// An action outside the vocabulary is an explicit rejection, never a silent default.
#[test]
fn test_unknown_steer_subtask_action_is_rejected() {
    for raw in [
        "",
        "   ",
        "remove",
        "not_an_action",
        "continue",
        "steer",
        "sleepy",
        "Cancel!",
        "forward_notice_x",
    ] {
        assert_eq!(
            normalize_steer_subtask_action(raw, "tc-1"),
            SteerSubtaskAction::Unknown,
            "{raw:?} must be rejected"
        );
        let err = raw
            .parse::<SteerSubtaskAction>()
            .expect_err("an action outside the vocabulary must fail FromStr");
        assert_eq!(err.raw, raw, "the raw spelling must be reported");
        assert!(
            err.to_string()
                .contains("unrecognized steer subtask action"),
            "rejection must name the problem: {err}"
        );
    }
    assert!(!SteerSubtaskAction::Unknown.is_known());
    assert_eq!(SteerSubtaskAction::Unknown.canonical(), "Unknown");
}

/// The rejection is logged, so a mis-spelled action cannot be silently swallowed.
#[test]
fn test_unknown_steer_subtask_action_rejection_is_logged() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Debug)]
    struct RejectionLogCounter(Arc<AtomicUsize>);

    struct MessageVisitor<'a> {
        found: &'a mut Option<String>,
    }

    impl tracing::field::Visit for MessageVisitor<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                *self.found = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.found = Some(format!("{value:?}"));
            }
        }
    }

    impl tracing::Subscriber for RejectionLogCounter {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.level() <= &tracing::Level::WARN
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::WARN)
        }

        fn register_callsite(
            &self,
            _metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().level() != &tracing::Level::WARN {
                return;
            }
            let mut found = None;
            event.record(&mut MessageVisitor { found: &mut found });
            if let Some(message) = found
                && (message.contains("Rejecting steer subtask action")
                    || message.contains("Rejected steer subtask action"))
            {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    let counter = Arc::new(AtomicUsize::new(0));
    let _tracing_lock = crate::orchestrator::workers::TEST_TRACING_MUTEX
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // Ensure the rejection callsite is registered with the callsite registry before
    // rebuilding the interest cache for our test subscriber.
    let _ = normalize_steer_subtask_action("unknown_init", "tc-init");
    tracing::subscriber::with_default(RejectionLogCounter(counter.clone()), || {
        tracing::callsite::rebuild_interest_cache();
        for raw in [
            "Cancel",
            "cancel_task",
            crate::tool_names::TOOL_DELEGATE_TASK,
            "forward_notice",
        ] {
            let _ = normalize_steer_subtask_action(raw, "tc-1");
        }
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "a recognized action must not log a rejection"
        );

        let _ = normalize_steer_subtask_action("destroy_everything", "tc-42");
        // The tracing interest cache is process-global: a test running on another thread can
        // install a different subscriber and leave this callsite cached as disabled, which
        // would make the assertion below fail on an otherwise correct implementation. The
        // rejection is therefore re-attempted (bounded) with the interest cache rebuilt, and
        // the *first* logged rejection satisfies "exactly one".
        let mut logged = counter.load(Ordering::Relaxed);
        for attempt in 0..5 {
            if logged > 0 {
                break;
            }
            if attempt > 0 {
                tracing::callsite::rebuild_interest_cache();
                let _ = normalize_steer_subtask_action("destroy_everything", "tc-42");
                logged = counter.load(Ordering::Relaxed);
            }
        }
        assert_eq!(
            logged, 1,
            "an unrecognized action must log exactly one explicit rejection"
        );
    });
    drop(_tracing_lock);
}

/// Every spelling of `DelegateTask` produces the same delegation, at the consumer.
#[test]
fn test_extract_tasks_to_delegate_accepts_action_spelling_variants() {
    for spelling in [
        "DelegateTask",
        crate::tool_names::TOOL_DELEGATE_TASK,
        "delegate",
        "DELEGATE TASK",
        "Delegate-Task",
        "new_task",
    ] {
        let json = format!(
            r#"{{"decision":"RespondDirectly","response":"ok","subtasks":[{{"tool_call_id":"[steer-task-1]","action":"{spelling}","agent_name":"researcher","prompt":"Run cargo test"}}]}}"#
        );
        let d: SteerDecision = serde_json::from_str(&json).unwrap();
        let tasks = extract_tasks_to_delegate(&d, "fallback prompt");
        assert_eq!(
            tasks.len(),
            1,
            "H3: action {spelling:?} must delegate a task"
        );
        assert_eq!(tasks[0].0, Agent::Researcher);
        assert_eq!(tasks[0].1, "steer-task-1");
        assert_eq!(tasks[0].2, "Run cargo test");
    }
}

/// A rejected action must not fall through to the implicit top-level delegation.
#[test]
fn test_extract_tasks_to_delegate_rejects_unknown_action() {
    let json = r#"{
        "decision": "DelegateTask",
        "response": "I will take care of it.",
        "subtasks": [
            {
                "tool_call_id": "t-001",
                "action": "destroy_task",
                "agent_name": "researcher",
                "prompt": "do the thing"
            }
        ]
    }"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    let tasks = extract_tasks_to_delegate(&d, "user instruction");
    assert!(
        tasks.is_empty(),
        "H3: an unrecognized action must be rejected, not silently routed to the default delegation branch, got {tasks:?}"
    );

    // The implicit fallback still works when there are no subtasks at all.
    let json = r#"{"decision":"DelegateTask","response":"ok","subtasks":[]}"#;
    let d: SteerDecision = serde_json::from_str(json).unwrap();
    assert_eq!(extract_tasks_to_delegate(&d, "run cargo check").len(), 1);
}

/// Regression pin: the sleep **tool** has exactly ONE alias vocabulary.
///
/// Every spelling of `crate::tool_names::TOOL_ALIAS_TABLE` naming the sleep tool
/// must be accepted by BOTH consumers — the harness dispatchers
/// (`harness::normalize_tool_name` + `tool_names::is_sleep_tool_name`) and the
/// steer subtask-action grammar (`normalize_steer_subtask_action` / `FromStr`)
/// — and a near-miss must be refused by both. A steer-private alias set would
/// disagree with the harness owner and split the vocabulary in two.
#[test]
fn sleep_tool_aliases_resolve_identically_on_the_harness_and_steer_paths() {
    let vocabulary = crate::tool_names::tool_spellings_for(crate::tool_names::TERMINAL_SLEEP);
    assert!(
        vocabulary.len() >= 6,
        "the shared sleep vocabulary must not shrink: {vocabulary:?}"
    );

    for spelling in &vocabulary {
        assert_eq!(
            crate::harness::normalize_tool_name(spelling),
            crate::tool_names::TERMINAL_SLEEP,
            "the harness gate must resolve the table spelling {spelling:?} to the sleep tool"
        );
        assert_eq!(
            crate::tool_names::canonical_tool_spelling(spelling),
            crate::tool_names::TERMINAL_SLEEP,
            "the shared table must resolve the table spelling {spelling:?} to the sleep tool"
        );
        assert!(
            crate::tool_names::is_sleep_tool_name(spelling),
            "harness dispatchers must classify {spelling:?} as the sleep tool"
        );
        assert!(
            crate::tool_names::is_sleep_tool_grammar_spelling(spelling),
            "the steer grammar-tolerant matcher must classify {spelling:?} as the sleep tool"
        );
        assert_eq!(
            normalize_steer_subtask_action(spelling, "tc-1"),
            SteerSubtaskAction::Sleep,
            "steer must map the table spelling {spelling:?} to Sleep"
        );
        assert_eq!(
            spelling.parse::<SteerSubtaskAction>(),
            Ok(SteerSubtaskAction::Sleep),
            "steer FromStr must map the table spelling {spelling:?} to Sleep"
        );
    }

    // Near-misses must stay outside the vocabulary on BOTH paths.
    for near_miss in [
        "sleepy",
        "asleep",
        "waits",
        "await",
        "paused",
        "nap",
        "sleeptaskx",
        "wait_second",
    ] {
        assert!(
            !crate::tool_names::is_sleep_tool_name(near_miss),
            "harness must not treat {near_miss:?} as the sleep tool"
        );
        assert_ne!(
            crate::harness::normalize_tool_name(near_miss),
            crate::tool_names::TERMINAL_SLEEP,
            "the harness gate must not fold {near_miss:?} to the sleep tool"
        );
        assert_ne!(
            normalize_steer_subtask_action(near_miss, "tc-1"),
            SteerSubtaskAction::Sleep,
            "steer must not map {near_miss:?} to Sleep"
        );
    }

    // The alias table is a *tool* vocabulary; it must not leak into the
    // subtask-action vocabulary of `normalize_steer_subtask_action`.
    for tool_alias in ["view_file", "bash", "pty__list", "edit_file", "grep"] {
        assert_ne!(
            normalize_steer_subtask_action(tool_alias, "tc-1"),
            SteerSubtaskAction::Sleep,
            "the tool alias {tool_alias:?} is not a steer action"
        );
    }
    for action in ["cancel", "delegate", "forward_notice"] {
        assert!(
            crate::tool_names::normalize_tool_alias(action).is_none(),
            "the subtask action {action:?} must not enter the tool-alias table"
        );
    }
}

// The steered sleep duration belongs to the sleep owner

/// Every `sleep`-argument shape a steered sleep can arrive with, and the number
/// of seconds the single owner resolves it to.
fn steered_sleep_cases() -> Vec<(&'static str, &'static str, u64)> {
    vec![
        ("no duration key", "{}", SLEEP_DEFAULT_SECS),
        (
            "float 1e9 -> default",
            r#"{"seconds": 1e9}"#,
            SLEEP_DEFAULT_SECS,
        ),
        (
            "int 999999 -> max",
            r#"{"seconds": 999999}"#,
            SLEEP_MAX_SECS,
        ),
        (
            "negative int -> default",
            r#"{"seconds": -3}"#,
            SLEEP_DEFAULT_SECS,
        ),
        ("string duration alias", r#"{"duration": "45"}"#, 45),
        (
            "duration_seconds alias",
            r#"{"duration_seconds": "30"}"#,
            30,
        ),
        ("int 0 -> min", r#"{"seconds": 0}"#, SLEEP_MIN_SECS),
        (
            "unparseable args -> default",
            "not json at all",
            SLEEP_DEFAULT_SECS,
        ),
        (
            "reason never shifts the duration",
            r#"{"seconds": 10, "reason": "wait for build"}"#,
            10,
        ),
    ]
}

/// The steered sleep must be resolved by the sleep-argument owner: same value
/// the owner returns for the same arguments, and always inside the owner's
/// bounds — an unbounded request such as `1e9`/`999999` must never reach the
/// caller as a sleep duration.
#[test]
fn steered_sleep_tool_call_resolves_through_the_sleep_owner() {
    for (label, arguments, expected) in steered_sleep_cases() {
        let decision = sleep_steer_decision(arguments);
        assert_eq!(
            decision.decision, "Sleep",
            "{label}: must stay a Sleep decision"
        );
        let secs = decision
            .sleep_seconds
            .unwrap_or_else(|| panic!("{label}: a steered sleep must carry a duration"));
        assert_eq!(
            secs, expected,
            "{label}: args {arguments} must resolve through the sleep owner"
        );
        // The exact same arguments resolved directly by the owner: the two paths
        // cannot disagree about what an input means.
        let parsed: serde_json::Value = serde_json::from_str(arguments).unwrap_or_default();
        assert_eq!(
            sleep_duration_secs(&parsed),
            secs,
            "{label}: steer and the sleep tool path disagree"
        );
        assert!(
            (SLEEP_MIN_SECS..=SLEEP_MAX_SECS).contains(&secs),
            "{label}: a steered sleep must never leave the owner's bounds, got {secs}"
        );
    }
}

/// The user-facing wording keeps naming the resolved (clamped/defaulted)
/// duration, and a `reason` is carried through without shifting it.
#[test]
fn steered_sleep_response_names_the_resolved_duration_and_reason() {
    let bare = sleep_steer_decision(r#"{"seconds": 999999}"#);
    assert_eq!(
        bare.response.as_deref(),
        Some("Sleeping for 300 seconds..."),
        "the message must name the clamped duration, not the requested one"
    );

    let reasoned = sleep_steer_decision(r#"{"seconds": 10, "reason": "wait for build"}"#);
    assert_eq!(
        reasoned.response.as_deref(),
        Some("Sleeping for 10 seconds (wait for build)...")
    );
    assert_eq!(reasoned.sleep_seconds, Some(10));

    let defaulted = sleep_steer_decision("{}");
    assert_eq!(defaulted.sleep_seconds, Some(SLEEP_DEFAULT_SECS));
    assert!(
        defaulted
            .response
            .as_deref()
            .unwrap()
            .contains(&SLEEP_DEFAULT_SECS.to_string()),
        "the default must be visible to the user too"
    );
}

/// Drive one sleep-shaped tool call through the real arbitration path (the SSE
/// reply carries a tool call and no decision JSON).
async fn steer_sleep_from_mock_tool_call(arguments: &str) -> SteerDecision {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let sse = serde_json::json!({
        "id": "chatcmpl-sleep",
        "choices": [{
            "delta": {"tool_calls": [{
                "index": 0,
                "id": "call_sleep_1",
                "type": "function",
                "function": {"name": crate::tool_names::TOOL_SLEEP, "arguments": arguments}
            }]},
            "finish_reason": serde_json::Value::Null
        }]
    });
    let body = format!("data: {sse}\n\ndata: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
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
        user_message: "wait a moment",
        active_subtasks: "None",
    };

    let decision = arbitrate_steer_context_stream(&client, &stats, ctx, |_| {}).await;
    decision.expect("a sleep tool call must yield a steer decision")
}

/// End-to-end through the arbitrator: an unbounded request is clamped and a
/// missing duration gets the owner's default — the values the wait is built from
/// can never exceed the owner's ceiling again.
#[tokio::test]
async fn test_arbitrate_steer_sleep_tool_call_never_exceeds_the_owner_ceiling() {
    for (label, arguments, expected) in steered_sleep_cases() {
        let d = steer_sleep_from_mock_tool_call(arguments).await;
        assert_eq!(d.decision, "Sleep", "{label}");
        assert_eq!(
            d.sleep_seconds,
            Some(expected),
            "{label}: the arbitrator path must resolve through the sleep owner"
        );
        assert!(
            d.sleep_seconds.is_none_or(|secs| secs <= SLEEP_MAX_SECS),
            "{label}: an unbounded steered sleep is the t-068 bug"
        );
    }
}

/// The JSON decision path: a `Sleep` without a duration gets the **owner's**
/// default constant, not a steer-local literal.
#[tokio::test]
async fn test_arbitrate_steer_json_sleep_without_duration_uses_the_owner_default() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let payload = serde_json::json!({"decision": "Sleep", "response": "Waiting a moment."});
    let sse = serde_json::json!({
        "id": "chatcmpl-json",
        "choices": [{"delta": {"content": payload.to_string()}, "finish_reason": "stop"}]
    });
    let body = format!("data: {sse}\n\ndata: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
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
        user_message: "hold on",
        active_subtasks: "None",
    };

    let d = arbitrate_steer_context_stream(&client, &stats, ctx, |_| {})
        .await
        .expect("a Sleep decision JSON must yield a decision");
    assert_eq!(d.decision, "Sleep");
    assert_eq!(d.sleep_seconds, Some(SLEEP_DEFAULT_SECS));
}

#[tokio::test]
async fn test_arbitrate_steer_context_stream_ignores_thinking_draft_json() {
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    let server = MockServer::start().await;
    let reasoning = "Drafting: {\"decision\": \"RespondDirectly\", \"response\": \"WRONG DRAFT FROM THINKING\"}";
    let payload = serde_json::json!({
        "decision": "RespondDirectly",
        "response": "Clean synthesized response to the user."
    });
    let sse1 = serde_json::json!({
        "id": "chatcmpl-think-1",
        "choices": [{"delta": {"reasoning_content": reasoning}, "finish_reason": null}]
    });
    let sse2 = serde_json::json!({
        "id": "chatcmpl-think-1",
        "choices": [{"delta": {"content": payload.to_string()}, "finish_reason": "stop"}]
    });
    let body = format!("data: {sse1}\n\ndata: {sse2}\n\ndata: [DONE]\n\n");

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body))
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
        user_message: "status?",
        active_subtasks: "None",
    };

    let mut streamed = String::new();
    let d = arbitrate_steer_context_stream(&client, &stats, ctx, |delta| {
        streamed.push_str(delta);
    })
    .await
    .expect("decision should parse cleanly after thinking block");

    assert_eq!(d.decision, "RespondDirectly");
    assert_eq!(
        d.response.as_deref(),
        Some("Clean synthesized response to the user.")
    );
    assert_eq!(streamed, "Clean synthesized response to the user.");
}

/// Source guard for the collapse: the production region of `steer.rs` must not
/// keep a sleep default/clamp literal (`unwrap_or(5)` + `Some(5)` without a
/// clamp), nor re-type the duration-key lookups or the arbitrator's own budget
/// knob — everything goes through `tool_args::sleep_duration_secs` / its
/// constants. Needles are assembled at runtime (style of the guards in
/// `harness::sleep` and `tool_names`) so this test's own text can never satisfy
/// them, and comment-only lines are excluded the way the marker guard excludes
/// them.
#[test]
fn steer_production_region_retypes_no_sleep_default_or_clamp_literals() {
    let src = include_str!("steer.rs");
    let production = src
        .split(&["#[cfg(", "test)", "]"].concat())
        .next()
        .expect("steer.rs always has a first region");
    let is_comment = |line: &str| {
        let trimmed = line.trim_start();
        trimmed.starts_with("//") || trimmed.starts_with("/*") || trimmed.starts_with('*')
    };
    let code: String = production
        .lines()
        .filter(|line| !is_comment(line))
        .collect::<Vec<&str>>()
        .join("\n");

    assert!(
        code.contains(&["fn sleep", "_steer_decision("].concat()),
        "the sleep wording must live in one helper, not inline in the arbitration chain"
    );

    for needle in [
        &["unwrap_or(", "5"].concat(),
        &["unwrap_or(", "300"].concat(),
        &["Some(", "5)"].concat(),
        &["clamp(", "1, "].concat(),
        &["clamp(", "300"].concat(),
        &["MIN_SINGLE_SLEEP", "_SECONDS"].concat(),
        &["get(\"sec", "onds\")"].concat(),
        &["get(\"dur", "ation\")"].concat(),
        &["get(\"duration_", "seconds\")"].concat(),
        "999999",
        "300",
    ] {
        assert!(
            !code.contains(needle),
            "steer.rs production must not re-type `{needle}`: the sleep default and \
             the min/max clamp belong to crate::tool_args, and the arbitrator's own \
             sleep budget belongs to ui::bridge::arbiter"
        );
    }

    // The owner call is assembled from the shared tool-name constant — no bare
    // tool-name literal in `src/`, and still no literal the guards above could
    // satisfy by accident.
    let owner_call = format!("{}_duration_secs(", crate::tool_names::TOOL_SLEEP);
    assert_eq!(
        code.matches(&owner_call).count(),
        1,
        "the steered sleep must call the single sleep-argument owner exactly once"
    );
    let default_const = ["SLEEP_", "DEFAULT_SECS"].concat();
    assert_eq!(
        code.matches(&default_const).count(),
        1,
        "the Sleep-without-duration fallback must name the owner's default constant"
    );
    assert!(
        code.contains(&["is_sleep_tool", "_name("].concat()),
        "the sleep-tool detection must keep reusing the crate::tool_names alias table"
    );
}
