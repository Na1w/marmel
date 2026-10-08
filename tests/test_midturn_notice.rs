//! Mid-turn steering-notice drain (task t-048).
//!
//! A steering notice posted **while a worker turn is in flight** must be drained
//! and injected inside that SAME turn — after the tool round, before compaction
//! and before a verdict/terminal break can end the loop — so it reaches the
//! worker's very next LLM request instead of waiting for a turn boundary the
//! worker may never cross again.
//!
//! These tests drive the real live loops (`run_specialist_live`) against a
//! wiremock backend, capture every request body the worker sends, and assert on
//! the *rendered* notice text produced by the crate's single renderer
//! (`marmennill::orchestrator::render_notice_for_worker`).
//!
//! Hermeticity: every run is scoped to an isolated temporary workspace root, so
//! the suite never reads or writes the repository's real `.marmel/`.

use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The three runs below share process-global worker/notice registries, so they
/// are serialized against each other.
static RUN_SEM: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

async fn run_lock() -> tokio::sync::SemaphorePermit<'static> {
    RUN_SEM
        .acquire()
        .await
        .expect("the run semaphore is never closed")
}

/// One streamed assistant turn carrying a single tool call.
fn tool_call_sse(tool: &str, call_id: &str, arguments: &str) -> String {
    let chunk = serde_json::json!({
        "choices": [{"delta": {"tool_calls": [{
            "index": 0,
            "id": call_id,
            "type": "function",
            "function": {"name": tool, "arguments": arguments},
        }]}}]
    });
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

/// One streamed assistant turn carrying only text.
fn content_sse(text: &str) -> String {
    let chunk = serde_json::json!({"choices": [{"delta": {"content": text}}]});
    format!("data: {chunk}\n\ndata: [DONE]\n\n")
}

/// The rendered notice as it must appear inside a JSON request body.
fn rendered_notice_in_json(notice_id: &str, inquiry: &str) -> String {
    let notice = marmennill::orchestrator::SteerNotice {
        notice_id: notice_id.to_string(),
        user_inquiry: inquiry.to_string(),
        target_worker: String::new(),
        created_at_ms: 0,
    };
    let rendered = marmennill::orchestrator::render_notice_for_worker(&notice);
    // The transcript is JSON-encoded in the request body, so compare against the
    // escaped rendering (minus the surrounding quotes).
    let escaped = serde_json::to_string(&rendered).expect("rendered notice serializes");
    escaped[1..escaped.len() - 1].to_string()
}

fn coder_config(
    server: &MockServer,
    specialist: &str,
    validator: bool,
) -> marmennill::config::Config {
    let mut specialists = std::collections::BTreeMap::new();
    specialists.insert(
        specialist.to_string(),
        marmennill::config::SpecialistConfig {
            enable_validator: Some(validator),
            ..Default::default()
        },
    );
    marmennill::config::Config {
        backend_url: format!("{}/v1", server.uri()),
        model: "test-model".to_string(),
        orchestration: marmennill::config::OrchestrationConfig {
            specialists,
            ..Default::default()
        },
        ..Default::default()
    }
}

/// A notice posted after the turn's own turn-start drain has already run — i.e.
/// while that turn is in flight — is injected into the SAME turn: it is present
/// in the very next request the worker sends, and there is no later request in
/// which it could have been deferred to.
#[tokio::test]
async fn test_notice_posted_mid_turn_reaches_the_same_turns_next_request() {
    let _run = run_lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root, async move {
        let server = MockServer::start().await;
        let worker_key = "coder-t-mt1";
        let notice_id = "notice-mt-inflight";
        let inquiry = "Mid-turn steer: keep the reader streaming";

        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));

        let responder_bodies = bodies.clone();
        let responder_calls = calls.clone();
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                responder_bodies.lock().unwrap().push(body);
                let n = responder_calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    // Posted WHILE this turn is in flight: this turn's turn-start
                    // drain has already run (and found nothing), and the tool
                    // round below is still executing when the turn ends.
                    marmennill::orchestrator::post_notice_to_worker(
                        worker_key,
                        inquiry,
                        Some(notice_id),
                    );
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        marmennill::tool_names::TOOL_GLOB,
                        "call_mt_1",
                        "{\"pattern\":\"*.rs\"}",
                    ));
                }
                ResponseTemplate::new(200).set_body_string(content_sse(&format!(
                    "{} (t-mt1)",
                    marmennill::markers::MARKER_COMPLETE
                )))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let request = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "List the rust files".to_string(),
            snippets: vec![],
            task_id: Some("t-mt1".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx =
            marmennill::agents::IsolatedContext::from_request("You are the Coder.".to_string(), &request);
        let token = tokio_util::sync::CancellationToken::new();

        marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &coder_config(&server, "coder", false),
            &token,
        )
        .await
        .expect("specialist run completes");

        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            2,
            "the run is exactly two requests: the tool round, then the concluding turn"
        );
        assert!(
            !bodies[0].contains(notice_id),
            "the notice was posted after this turn's turn-start drain, so it cannot be in the first request"
        );
        assert!(
            bodies[1].contains(&rendered_notice_in_json(notice_id, inquiry)),
            "the notice posted mid-turn must be injected, byte-identical to the single \
             renderer's output, in the SAME turn's next request; got: {bodies:?}"
        );
        // Consolidation evidence: the removed inline copy of the rendering told
        // the worker to reply with a single-quoted tool name and never pinned the
        // notice id; only the canonical renderer may appear.
        let stale_contract = format!("invoke the '{}' tool", marmennill::tool_names::TOOL_REPLY_TO_ARBITRATOR);
        assert!(
            !bodies[1].contains(&stale_contract),
            "the deprecated inline notice rendering must no longer reach the model"
        );
    })
    .await;
}

/// The stronger same-turn claim: a turn that ENDS at its tool round (a validator
/// role concluding with the verdict tool) never runs another turn-start drain, so
/// a notice posted during that turn can only have been injected by the mid-turn
/// drain. Before t-048 such a notice stayed queued until the TTL sweep dropped it.
#[tokio::test]
async fn test_notice_posted_mid_turn_is_injected_before_a_verdict_turn_can_conclude() {
    let _run = run_lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root, async move {
        let server = MockServer::start().await;
        let worker_key = "validator-t-mt2";
        let notice_id = "notice-mt-verdict";

        let calls = Arc::new(AtomicUsize::new(0));
        let calls_responder = calls.clone();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |_req: &Request| {
                let n = calls_responder.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    marmennill::orchestrator::post_notice_to_worker(
                        worker_key,
                        "check the failing test first",
                        Some(notice_id),
                    );
                }
                // The validator role concludes with the verdict tool: the tool
                // round IS the end of the turn.
                ResponseTemplate::new(200).set_body_string(tool_call_sse(
                    marmennill::tool_names::TOOL_LEAVE_VERDICT,
                    "call_mt_verdict",
                    "{\"verdict\":\"APPROVED\",\"comments\":\"verified in isolation\"}",
                ))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let request = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Validator,
            prompt: "Validate the deliverable".to_string(),
            snippets: vec![],
            task_id: Some("t-mt2".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Validator.".to_string(),
            &request,
        );
        let token = tokio_util::sync::CancellationToken::new();

        marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Validator,
            &ctx,
            &coder_config(&server, "validator", false),
            &token,
        )
        .await
        .expect("validator run completes");

        assert_eq!(
            calls.load(Ordering::SeqCst),
            1,
            "the verdict tool ends the turn, so this worker sends no further request"
        );
        assert_eq!(
            marmennill::orchestrator::notice::worker_inbox_len(worker_key),
            0,
            "the notice posted during the turn must have been drained by the mid-turn drain \
             inside that same turn — no later turn-start drain exists for this worker"
        );
        assert!(
            marmennill::orchestrator::get_pending_notice(notice_id).is_some(),
            "draining a notice delivers it; it is never discarded before the worker replies"
        );
    })
    .await;
}

/// Routing is exact-identity: a notice addressed to a DIFFERENT worker is never
/// injected into this worker's transcript, mid-turn or otherwise, and it stays
/// queued for the worker it actually addresses.
#[tokio::test]
async fn test_notice_addressed_to_another_worker_is_not_injected() {
    let _run = run_lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root, async move {
        let server = MockServer::start().await;
        let foreign_key = "coder-t-mt9";
        let notice_id = "notice-mt-foreign";
        let inquiry = "This one is not for you";

        let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let calls = Arc::new(AtomicUsize::new(0));
        let responder_bodies = bodies.clone();
        let responder_calls = calls.clone();

        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                responder_bodies.lock().unwrap().push(body);
                let n = responder_calls.fetch_add(1, Ordering::SeqCst);
                if n == 0 {
                    marmennill::orchestrator::post_notice_to_worker(
                        foreign_key,
                        inquiry,
                        Some(notice_id),
                    );
                    return ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        marmennill::tool_names::TOOL_GLOB,
                        "call_mt_foreign",
                        "{\"pattern\":\"*.rs\"}",
                    ));
                }
                ResponseTemplate::new(200).set_body_string(content_sse(&format!(
                    "{} (t-mt3)",
                    marmennill::markers::MARKER_COMPLETE
                )))
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let request = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "List the rust files".to_string(),
            snippets: vec![],
            task_id: Some("t-mt3".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(
            "You are the Coder.".to_string(),
            &request,
        );
        let token = tokio_util::sync::CancellationToken::new();

        marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &coder_config(&server, "coder", false),
            &token,
        )
        .await
        .expect("specialist run completes");

        let bodies = bodies.lock().unwrap().clone();
        assert_eq!(
            bodies.len(),
            2,
            "the run makes the same two requests as before"
        );
        for body in &bodies {
            assert!(
                !body.contains(notice_id) && !body.contains(inquiry),
                "a notice addressed to {foreign_key} must never be injected for coder-t-mt3"
            );
        }
        assert_eq!(
            marmennill::orchestrator::notice::worker_inbox_len(foreign_key),
            1,
            "the notice stays queued for the worker it addresses"
        );
    })
    .await;
}
