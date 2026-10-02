use super::notice::*;
use crate::agents::Agent;
use crate::harness::{HarnessStats, ToolCaller, ToolInvocation, dispatch_for_async};
use crate::llm::ChatClient;
use crate::orchestrator::SpecialistRegistry;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

#[tokio::test]
async fn test_notice_id_generation_and_posting() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice1 = post_notice_to_worker("coder", "Why did you use SQLite?", None);
    let notice2 = post_notice_to_worker("debugger", "Check stack trace on line 42", None);

    assert!(notice1.notice_id.starts_with("notice-"));
    assert!(notice2.notice_id.starts_with("notice-"));
    assert_ne!(notice1.notice_id, notice2.notice_id);

    assert_eq!(notice1.user_inquiry, "Why did you use SQLite?");
    assert_eq!(notice1.target_worker, "coder");

    assert_eq!(notice2.user_inquiry, "Check stack trace on line 42");
    assert_eq!(notice2.target_worker, "debugger");

    assert!(get_pending_notice(&notice1.notice_id).is_some());
    assert!(get_pending_notice(&notice2.notice_id).is_some());
}

#[tokio::test]
async fn test_worker_inbox_draining_with_prefix_matching() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let n1 = post_notice_to_worker("coder", "Question 1 for coder", None);
    let _n2 = post_notice_to_worker("debugger", "Question for debugger", None);

    // Draining for worker key "coder-t-001" should match target "coder"
    let drained = drain_worker_notices("coder-t-001");
    assert_eq!(drained.len(), 1);
    assert_eq!(drained[0].notice_id, n1.notice_id);
    assert_eq!(drained[0].user_inquiry, "Question 1 for coder");

    // Second drain should be empty
    let empty = drain_worker_notices("coder-t-001");
    assert!(empty.is_empty());

    // Draining for debugger
    let drained_dbg = drain_worker_notices("debugger");
    assert_eq!(drained_dbg.len(), 1);
    assert_eq!(drained_dbg[0].user_inquiry, "Question for debugger");
}

#[tokio::test]
async fn test_record_worker_reply_matches_pending_notice() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let n = post_notice_to_worker("coder", "Why SQLite?", Some("notice-custom-1"));
    assert_eq!(n.notice_id, "notice-custom-1");

    let res = record_worker_reply(
        "coder-t-001",
        "notice-custom-1",
        "Because it requires no external server setup.",
    );

    assert!(res.is_ok());
    let original = res.unwrap();
    assert_eq!(original.user_inquiry, "Why SQLite?");
    assert_eq!(original.notice_id, "notice-custom-1");

    // Pending notice is removed once replied
    assert!(get_pending_notice("notice-custom-1").is_none());

    // Reply is recorded in store
    let reply = get_worker_reply("notice-custom-1");
    assert!(reply.is_some());
    let rep = reply.unwrap();
    assert_eq!(rep.notice_id, "notice-custom-1");
    assert_eq!(rep.worker_tag, "coder-t-001");
    assert_eq!(
        rep.reply_message,
        "Because it requires no external server setup."
    );
}

#[tokio::test]
async fn test_handle_reply_to_arbitrator_success_and_validation() {
    let _lock = TEST_NOTICE_MUTEX.lock().await;
    clear_all_notices();

    let notice = post_notice_to_worker("coder", "Please use async I/O", Some("notice-async-1"));

    // Missing notice_id should fail with BadArguments
    let tool_missing_id = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "message": "Done"
        }),
    };
    let err_missing_id =
        dispatch_for_async(&tool_missing_id, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(err_missing_id.is_err());

    // Missing message should fail with BadArguments
    let tool_missing_msg = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-async-1"
        }),
    };
    let err_missing_msg =
        dispatch_for_async(&tool_missing_msg, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(err_missing_msg.is_err());

    // Valid reply
    let tool_valid = ToolInvocation {
        name: crate::tool_names::TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-async-1",
            "message": "Switched to tokio::fs for all file operations."
        }),
    };
    let ok_res = dispatch_for_async(&tool_valid, ToolCaller::Specialist(Agent::Coder)).await;
    assert!(ok_res.is_ok());
    let out = ok_res.unwrap();
    assert!(!out.is_error);
    assert!(out.content.contains("recorded successfully"));
    assert!(out.content.contains(&notice.user_inquiry));

    // Notice is now resolved
    assert!(get_pending_notice("notice-async-1").is_none());
    assert!(get_worker_reply("notice-async-1").is_some());
}

fn sse_delta(content: &str) -> String {
    let obj = serde_json::json!({
        "id": "chatcmpl-1",
        "choices": [{
            "delta": {
                "content": content
            },
            "finish_reason": null
        }]
    });
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&obj).unwrap()
    )
}

#[tokio::test]
async fn test_evaluate_worker_reply_synthesize_response() {
    let server = MockServer::start().await;

    let response_json = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "Coder förklarar att SQLite valdes för att minimera externa beroenden.",
        "follow_up_prompt": null,
        "user_status": null
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_delta(&response_json.to_string())),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock-model", "tok");
    let stats = HarnessStats::new();

    let notice = SteerNotice {
        notice_id: "notice-eval-1".to_string(),
        user_inquiry: "Varför valde du SQLite?".to_string(),
        target_worker: "coder".to_string(),
        created_at_ms: 100,
    };

    let mut streamed_deltas = Vec::new();
    let eval = evaluate_worker_reply(
        &client,
        &stats,
        &notice,
        "coder-t-001",
        "SQLite har noll konfiguration och fungerar utmärkt med WAL-mode.",
        |delta| streamed_deltas.push(delta.to_string()),
    )
    .await
    .expect("evaluation succeeds");

    assert_eq!(eval.decision, "SynthesizeResponse");
    assert_eq!(
        eval.response.as_deref(),
        Some("Coder förklarar att SQLite valdes för att minimera externa beroenden.")
    );
    assert!(eval.follow_up_prompt.is_none());
    assert!(!streamed_deltas.is_empty());
}

#[tokio::test]
async fn test_evaluate_worker_reply_ask_followup() {
    let server = MockServer::start().await;

    let response_json = serde_json::json!({
        "decision": "AskFollowUp",
        "response": null,
        "follow_up_prompt": "Will switching to SQLite break the transaction isolation requirements in requirement t-003?",
        "user_status": "Ställer en följdfråga till Coder gällande transaktionsisolering..."
    });

    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(
            ResponseTemplate::new(200).set_body_string(sse_delta(&response_json.to_string())),
        )
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock-model", "tok");
    let stats = HarnessStats::new();

    let notice = SteerNotice {
        notice_id: "notice-eval-2".to_string(),
        user_inquiry: "Kan vi byta till SQLite?".to_string(),
        target_worker: "coder".to_string(),
        created_at_ms: 200,
    };

    let mut streamed_deltas = Vec::new();
    let eval = evaluate_worker_reply(
        &client,
        &stats,
        &notice,
        "coder-t-001",
        "Jag kan byta databas.",
        |delta| streamed_deltas.push(delta.to_string()),
    )
    .await
    .expect("evaluation succeeds");

    assert_eq!(eval.decision, "AskFollowUp");
    assert!(eval.response.is_none());
    assert_eq!(
        eval.follow_up_prompt.as_deref(),
        Some(
            "Will switching to SQLite break the transaction isolation requirements in requirement t-003?"
        )
    );
    assert_eq!(
        eval.user_status.as_deref(),
        Some("Ställer en följdfråga till Coder gällande transaktionsisolering...")
    );
    // When follow-up is requested, no final response is streamed to the user yet
    assert!(streamed_deltas.is_empty());
}

#[tokio::test]
async fn test_evaluate_worker_reply_fallback_on_plain_text() {
    let server = MockServer::start().await;

    // LLM outputs non-JSON plain text
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_delta(
            "Specialist Coder explains that timeout is already set to 15s.",
        )))
        .mount(&server)
        .await;

    let client = ChatClient::new_with_token(server.uri(), "mock-model", "tok");
    let stats = HarnessStats::new();

    let notice = SteerNotice {
        notice_id: "notice-eval-3".to_string(),
        user_inquiry: "Did you set a timeout?".to_string(),
        target_worker: "coder".to_string(),
        created_at_ms: 300,
    };

    let mut streamed_deltas = Vec::new();
    let eval = evaluate_worker_reply(
        &client,
        &stats,
        &notice,
        "coder-t-001",
        "Timeout is 15s.",
        |delta| streamed_deltas.push(delta.to_string()),
    )
    .await
    .expect("fallback succeeds");

    assert_eq!(eval.decision, "SynthesizeResponse");
    assert!(eval.response.is_some());
    assert!(eval.response.unwrap().contains("timeout is already set"));
    assert!(!streamed_deltas.is_empty());
}

#[test]
fn test_all_agent_archetypes_allow_reply_to_arbitrator() {
    let registry = SpecialistRegistry::canonical();
    let all_agents = [
        Agent::Coder,
        Agent::Researcher,
        Agent::Debugger,
        Agent::Validator,
        Agent::Generalist,
        Agent::Planner,
    ];

    for agent in all_agents {
        assert!(
            crate::orchestrator::caller_allows_tool(
                agent,
                crate::tool_names::TOOL_REPLY_TO_ARBITRATOR,
                &registry
            ),
            "Agent {:?} must be allowed to invoke reply_to_arbitrator",
            agent
        );
    }
}

#[test]
fn test_assemble_tools_includes_reply_to_arbitrator_unconditionally() {
    // Even with a restricted allowed_tools list, reply_to_arbitrator is always included
    let restricted = vec![crate::tool_names::TOOL_READ_FILE.to_string()];
    let tools = crate::agents::runner::fix_loop::assemble_tools(Some(&restricted), |_| false, &[]);

    let has_reply = tools
        .iter()
        .any(|t| t.function.name == crate::tool_names::TOOL_REPLY_TO_ARBITRATOR);
    assert!(
        has_reply,
        "assemble_tools must always include reply_to_arbitrator"
    );
}
