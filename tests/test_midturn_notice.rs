//! Mid-turn steering-notice drain.
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
//! The last test closes the loop in the other direction: a `reply_to_arbitrator`
//! tool call issued **inside** such a runner-registered turn, the arbitrator turn
//! it triggers, the answer that reaches the user, and the follow-up hop of the
//! same dialogue delivered back to that same worker's next request.
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
        // Only the canonical renderer may reach the model: its output pins the
        // notice id and never uses the single-quoted tool-name phrasing asserted
        // absent below.
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
/// drain.
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

// --------------------------------------------------------------------------------
// The reply round trip **inside a real runner turn**.
//
// Everything above proves how a notice reaches a worker's transcript. This test closes the
// other half of the loop through the same real `run_specialist_live` machinery: the worker is
// registered by the runner itself (its own cancel token, `CURRENT_WORKER_TOKEN` scoped around
// the whole turn), it issues a real `reply_to_arbitrator` tool call naming the id it was
// handed, the arbitrator turn runs inside that tool round, its answer reaches the user, and
// the follow-up hop of the same dialogue is delivered back to **that** worker's next request.
// --------------------------------------------------------------------------------

/// True for the arbitrator's evaluation of a worker reply (`evaluate_worker_reply`), never for
/// a specialist turn: that request is the only one carrying this prompt header.
const ARBITRATOR_TURN_MARKER: &str = "Original User Inquiry:";

/// One full dialogue in a live runner turn:
/// `ForwardNotice`-shaped notice -> worker reply -> arbitrator turn (follow-up) -> follow-up
/// delivered to the SAME worker -> second worker reply -> arbitrator turn -> exactly one
/// user-visible answer.
#[tokio::test]
async fn test_reply_to_arbitrator_round_trip_runs_inside_a_real_runner_turn() {
    let _run = run_lock().await;

    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let root = tmp.path().to_path_buf();

    marmennill::harness::with_workspace_root(root, async move {
        let worker_key = "coder-t-rt1";
        let notice_id = "notice-rt-roundtrip";
        // What the bridge files: the arbitrator's rewrite of the user's steer, never the raw text.
        let rewritten_inquiry = "How is the parser refactor going?";
        let follow_up = "Did you run the crate test suite?";
        let final_answer = "Coder finished the refactor and the suite is green.";

        marmennill::orchestrator::clear_all_notices();

        // The user-facing side of the round trip: the arbitrator's answer is an
        // `Event::SteerResponse`, and the dialogue lives in the session's steering history.
        let steering_history: marmennill::orchestrator::SharedSteeringHistory =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
        marmennill::orchestrator::set_steering_history(std::sync::Arc::clone(&steering_history));
        let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
        marmennill::orchestrator::set_event_sender(event_tx);

        // Round one, filed under the key the RUNNER is about to register for this task id.
        marmennill::orchestrator::post_notice_to_worker(
            worker_key,
            rewritten_inquiry,
            Some(notice_id),
        );

        let server = MockServer::start().await;
        // The arbitrator turn reads the process-global active config, so the mock backend has to
        // be installed for it too.
        marmennill::config::set_active(marmennill::config::Config {
            backend_url: format!("{}/v1", server.uri()),
            model: "test-model".to_string(),
            ..Default::default()
        });

        let worker_bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let arb_bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let worker_turns = Arc::new(AtomicUsize::new(0));
        let arb_turns = Arc::new(AtomicUsize::new(0));

        let wb = Arc::clone(&worker_bodies);
        let ab = Arc::clone(&arb_bodies);
        let wt = Arc::clone(&worker_turns);
        let at = Arc::clone(&arb_turns);
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                let body = String::from_utf8_lossy(&req.body).to_string();
                if body.contains(ARBITRATOR_TURN_MARKER) {
                    ab.lock().unwrap().push(body);
                    let n = at.fetch_add(1, Ordering::SeqCst);
                    let decision = if n == 0 {
                        // Round one: the arbitrator is not satisfied and asks a follow-up.
                        serde_json::json!({
                            "decision": "AskFollowUp",
                            "response": null,
                            "follow_up_prompt": follow_up,
                            "user_status": "Asking the coder about the suite..."
                        })
                    } else {
                        serde_json::json!({
                            "decision": "SynthesizeResponse",
                            "response": final_answer,
                            "follow_up_prompt": null,
                            "user_status": null
                        })
                    };
                    return ResponseTemplate::new(200)
                        .set_body_string(content_sse(&decision.to_string()));
                }

                wb.lock().unwrap().push(body);
                match wt.fetch_add(1, Ordering::SeqCst) {
                    // Worker turn 1: answer the notice the turn-start drain handed it.
                    0 => ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        marmennill::tool_names::TOOL_REPLY_TO_ARBITRATOR,
                        "call_rt_1",
                        &serde_json::to_string(&serde_json::json!({
                            "notice_id": notice_id,
                            "message": "Refactor done, tokio::fs throughout."
                        }))
                        .expect("tool arguments serialize"),
                    )),
                    // Worker turn 2: answer the follow-up of the SAME dialogue.
                    1 => ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        marmennill::tool_names::TOOL_REPLY_TO_ARBITRATOR,
                        "call_rt_2",
                        &serde_json::to_string(&serde_json::json!({
                            "notice_id": notice_id,
                            "message": "Yes: cargo test is green, 214 passed."
                        }))
                        .expect("tool arguments serialize"),
                    )),
                    // Worker turn 3: conclude the turn.
                    _ => ResponseTemplate::new(200).set_body_string(content_sse(&format!(
                        "{} (t-rt1)",
                        marmennill::markers::MARKER_COMPLETE
                    ))),
                }
            })
            .mount(&server)
            .await;

        let client = marmennill::llm::ChatClient::new(format!("{}/v1", server.uri()), "test-model");
        let request = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Refactor the parser to async I/O".to_string(),
            snippets: vec![],
            task_id: Some("t-rt1".to_string()),
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

        let worker_bodies = worker_bodies.lock().unwrap().clone();
        let arb_bodies = arb_bodies.lock().unwrap().clone();

        // Three worker turns (reply, reply again, conclude) and exactly one arbitrator turn per
        // accepted reply — no second evaluation for one reply.
        assert_eq!(
            worker_bodies.len(),
            3,
            "the worker sends one request per turn, got {} request bodies",
            worker_bodies.len()
        );
        assert_eq!(
            arb_bodies.len(),
            2,
            "two accepted replies must cost exactly two arbitrator turns, got {arb_bodies:?}"
        );

        // The worker was handed round one by the runner's own drain, with the id it has to echo.
        assert!(
            worker_bodies[0].contains(&rendered_notice_in_json(notice_id, rewritten_inquiry)),
            "turn 1 must carry the delivered notice block, got: {worker_bodies:?}"
        );
        // Round two reached the SAME live worker, inside its own turn.
        assert!(
            worker_bodies[1].contains(&rendered_notice_in_json(notice_id, follow_up)),
            "the follow-up of the dialogue must be injected into the addressed worker's own \
             next request, got: {worker_bodies:?}"
        );

        // The worker got the tool results it was promised and was never rejected. The dialogue
        // id is reused for round two by design, so any rejection here is the RC-1/RC-3
        // regression. Only the dispatcher's own rejection wording counts: the tool *schema*
        // names the rejection classes, so it appears in every worker request.
        assert!(
            !worker_bodies.iter().any(|b| b.contains("reply rejected (")),
            "neither reply in a live two-round dialogue may be rejected by the reply contract"
        );
        assert!(
            worker_bodies[1].contains("Arbitrator has posted follow-up question"),
            "round one's tool result must report the follow-up that was posted"
        );
        assert!(
            worker_bodies[2].contains("recorded successfully"),
            "round two's tool result must report the reply was recorded"
        );

        // Each arbitrator turn saw exactly the reply it was answering.
        assert!(arb_bodies[0].contains("Refactor done, tokio::fs throughout."));
        assert!(!arb_bodies[0].contains("214 passed"));
        assert!(arb_bodies[1].contains("214 passed"));

        // The notice store is left clean: the dialogue closed, nothing is queued, nothing sits
        // pending behind a 10-minute TTL.
        assert!(
            marmennill::orchestrator::get_pending_notice(notice_id).is_none(),
            "the answered notice must not stay pending"
        );
        assert_eq!(
            marmennill::orchestrator::notice::worker_inbox_len(worker_key),
            0,
            "the addressed worker's inbox must be empty once the dialogue is answered"
        );
        assert_eq!(
            marmennill::orchestrator::get_worker_reply(notice_id)
                .expect("the last reply must be recorded")
                .reply_message,
            "Yes: cargo test is green, 214 passed."
        );

        // (c) Exactly one answer reaches the user, and the follow-up round is announced once.
        let mut answers: Vec<String> = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let marmennill::ui::Event::SteerResponse(text) = event {
                answers.push(text);
            }
        }
        let final_round: Vec<&String> = answers
            .iter()
            .filter(|text| text.contains(final_answer))
            .collect();
        assert_eq!(
            final_round.len(),
            1,
            "one worker reply must produce exactly one arbitrator answer, got {answers:?}"
        );
        let follow_up_round: Vec<&String> = answers
            .iter()
            .filter(|text| text.contains("Asking the coder about the suite"))
            .collect();
        assert_eq!(
            follow_up_round.len(),
            1,
            "the follow-up round must be surfaced to the user exactly once, got {answers:?}"
        );

        // One dialogue, one steering-history entry, and it is resolved — never left
        // "awaiting specialist reply".
        let history = steering_history.read().unwrap().clone();
        assert_eq!(history.len(), 1, "one dialogue, one entry: {history:?}");
        assert_eq!(history[0].0, rewritten_inquiry);
        assert!(history[0].1.contains(final_answer));
        assert!(
            !history[0].1.contains("awaiting specialist reply"),
            "an answered dialogue must not stay pending, got {}",
            history[0].1
        );
    })
    .await;
}
