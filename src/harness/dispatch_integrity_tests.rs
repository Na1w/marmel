//! Dispatch-layer integrity.
//!
//! Two rules that the dispatcher itself must enforce:
//!
//! 1. **Reply targeting.** A `reply_to_arbitrator` call must name the notice it
//!    answers, and that notice must address the replying worker. A rejection is
//!    surfaced to the specialist as a `ToolError` naming the rejection kind; the
//!    notice stays pending, nothing is recorded, and the dispatcher never
//!    synthesizes a stand-in notice or answers the user from a notice nobody named.
//! 2. **Verdict identity.** `leave_verdict` is refused at dispatch for every role
//!    except the validator, even when the role's allowlist admits it (the
//!    Generalist namespace is `"*"`, and a blueprint allowlist can name the tool
//!    for any role).
//!
//! Plus the output-shape guarantee: `read_file`'s `[read_file truncated]` report
//! survives the generic tool-output clip, at dispatch and in isolation.
//!
//! All assertions are per-notice-id / per-caller, never on process-global
//! counters: other modules post notices concurrently.

use super::*;
use crate::agents::Agent;
use crate::orchestrator::{
    clear_all_notices, get_pending_notice, get_worker_reply, post_notice_to_worker,
};
use crate::tool_names::{TOOL_LEAVE_VERDICT, TOOL_READ_FILE, TOOL_REPLY_TO_ARBITRATOR};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Lock shared with the notice-layer tests so the (process-global) notice store is
/// not mutated underneath the per-id assertions here.
async fn notice_lock() -> tokio::sync::MutexGuard<'static, ()> {
    crate::orchestrator::notice::TEST_NOTICE_MUTEX.lock().await
}

/// Restore the previous active config when the guard goes away: the arbitrator
/// turn reads a process-global config, and a mock backend must not leak into
/// other tests.
struct ActiveConfigGuard {
    previous: Option<crate::config::Config>,
}

impl ActiveConfigGuard {
    fn with_backend(base_url: String) -> Self {
        let previous = crate::config::get_active();
        crate::config::set_active(crate::config::Config {
            backend_url: base_url,
            ..Default::default()
        });
        Self { previous }
    }
}

impl Drop for ActiveConfigGuard {
    fn drop(&mut self) {
        if let Some(cfg) = self.previous.take() {
            crate::config::set_active(cfg);
        }
    }
}

/// One streamed SSE frame carrying `content` — the shape the arbitrator client
/// parses.
fn sse_delta(content: &str) -> String {
    let obj = serde_json::json!({
        "id": "chatcmpl-t051",
        "choices": [{ "delta": { "content": content }, "finish_reason": null }]
    });
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::to_string(&obj).unwrap()
    )
}

// 1. Reply targeting

#[tokio::test]
async fn test_reply_naming_another_workers_notice_is_rejected_and_stays_pending() {
    let _notice_lock = notice_lock().await;
    clear_all_notices();

    let foreign = post_notice_to_worker(
        "debugger-t-0511",
        "Explain the segfault frame",
        Some("notice-t051-foreign"),
    );

    let invocation = ToolInvocation {
        name: TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-t051-foreign",
            "message": "answering a notice that belongs to another worker"
        }),
    };
    let res = dispatch_for_async(&invocation, ToolCaller::Specialist(Agent::Coder)).await;

    // (a) an error reaches the specialist, and it names the rejection kind.
    let err = match res {
        Err(err) => err,
        Ok(ok) => panic!(
            "a reply naming another worker's notice must be refused, got ok: {:?}",
            ok.content
        ),
    };
    let detail = err.to_string();
    assert!(
        detail.contains("worker-mismatch"),
        "the error must name the rejection kind, got: {detail}"
    );
    assert!(
        detail.contains("notice-t051-foreign"),
        "the error must name the offending id, got: {detail}"
    );

    // (b) the notice is untouched: still pending, still addressed to its worker,
    // and no reply was recorded against it.
    let pending = get_pending_notice("notice-t051-foreign").expect("notice must stay pending");
    assert_eq!(pending.notice_id, foreign.notice_id);
    assert_eq!(pending.target_worker, foreign.target_worker);
    assert_eq!(pending.user_inquiry, foreign.user_inquiry);
    assert!(get_worker_reply("notice-t051-foreign").is_none());
}

#[tokio::test]
async fn test_reply_without_notice_id_is_rejected_and_no_notice_is_guessed() {
    let _notice_lock = notice_lock().await;
    clear_all_notices();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::orchestrator::set_event_sender(event_tx);

    // Exactly one pending notice for this worker: the historical
    // "take the single pending notice" behaviour would have resolved it here.
    let mine = post_notice_to_worker(
        "coder-t-0512",
        "Why the hand-written parser?",
        Some("notice-t051-guess"),
    );

    let invocation = ToolInvocation {
        name: TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "message": "it needs no external server"
        }),
    };
    let res = dispatch_for_async(&invocation, ToolCaller::Specialist(Agent::Coder)).await;
    let err = match res {
        Err(err) => err,
        Ok(ok) => panic!(
            "a reply with no notice id must be refused, got ok: {:?}",
            ok.content
        ),
    };
    let detail = err.to_string();
    assert!(
        detail.contains("missing-notice-id"),
        "the error must name the rejection kind, got: {detail}"
    );

    // No notice was guessed, consumed, or synthesized.
    let pending = get_pending_notice("notice-t051-guess").expect("notice must stay pending");
    assert_eq!(pending.notice_id, mine.notice_id);
    assert_eq!(pending.user_inquiry, "Why the hand-written parser?");
    assert!(get_worker_reply("notice-t051-guess").is_none());

    // No arbitrator turn: nothing was emitted to the user, so there is no
    // `Event::SteerResponse`-style success for a reply that was never recorded.
    match event_rx.try_recv() {
        Err(tokio::sync::mpsc::error::TryRecvError::Empty)
        | Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => {}
        Ok(event) => panic!("a rejected reply must emit no UI event, got {event:?}"),
    }
}

#[tokio::test]
async fn test_reply_with_explicit_notice_id_resolves_and_records_the_reply() {
    let _notice_lock = notice_lock().await;
    clear_all_notices();

    let server = MockServer::start().await;
    let decision = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "Understood: the parser stays.",
        "follow_up_prompt": null,
        "user_status": null
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_delta(&decision.to_string())))
        .mount(&server)
        .await;
    let _config = ActiveConfigGuard::with_backend(server.uri());

    // Addressed with a disambiguated registry key: the reply identity dispatch
    // can see (the caller role) still resolves it through the notice layer's
    // bidirectional routing rules.
    let posted = post_notice_to_worker(
        "coder-t-0513#7",
        "Why tokio rather than async-std?",
        Some("notice-t051-resolve"),
    );

    let invocation = ToolInvocation {
        name: TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-t051-resolve",
            "message": "tokio is already in the dependency tree"
        }),
    };
    let res = dispatch_for_async(&invocation, ToolCaller::Specialist(Agent::Coder))
        .await
        .expect("a correctly targeted reply must be accepted");
    assert!(!res.is_error);

    // The named notice was the one consumed, and the reply text was recorded.
    assert!(get_pending_notice("notice-t051-resolve").is_none());
    let reply = get_worker_reply("notice-t051-resolve").expect("reply must be recorded");
    assert_eq!(reply.notice_id, posted.notice_id);
    assert_eq!(
        reply.reply_message,
        "tokio is already in the dependency tree"
    );
}

#[tokio::test]
async fn test_reply_to_an_unknown_notice_id_is_rejected_without_state_change() {
    let _notice_lock = notice_lock().await;
    clear_all_notices();

    let mine = post_notice_to_worker(
        "coder-t-0514",
        "Progress check",
        Some("notice-t051-unknown-probe"),
    );

    let invocation = ToolInvocation {
        name: TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-t051-never-posted",
            "message": "replying to a notice that does not exist"
        }),
    };
    let res = dispatch_for_async(&invocation, ToolCaller::Specialist(Agent::Coder)).await;
    let err = match res {
        Err(err) => err,
        Ok(ok) => panic!(
            "an unknown notice id must be refused, got ok: {:?}",
            ok.content
        ),
    };
    let detail = err.to_string();
    assert!(
        detail.contains("unknown-notice-id"),
        "the error must name the rejection kind, got: {detail}"
    );

    // The worker's own notice is untouched — the unknown id cannot be traded for
    // the nearest pending one.
    let pending =
        get_pending_notice("notice-t051-unknown-probe").expect("notice must stay pending");
    assert_eq!(pending.notice_id, mine.notice_id);
    assert!(get_worker_reply("notice-t051-unknown-probe").is_none());
    assert!(get_worker_reply("notice-t051-never-posted").is_none());
}

// 1b. The **whole round trip**: notice -> worker reply -> one arbitrator turn ->
// user-visible answer, and the follow-up hop of that same dialogue.
//
// These two tests were the blind spot: every other test here posts its notice by hand under a
// role-shaped target and never registers a worker, so neither the reply identity (RC-4) nor the
// follow-up target (RC-3) could ever go wrong. A live worker is registered here with its own
// cancellation token, the reply is dispatched inside the same `CURRENT_WORKER_TOKEN` scope the
// specialist runner uses, and assertions are made on the **effective registry key** the worker
// drains its inbox with.

/// Install a private steering history for one test and restore the previous one on drop: the
/// arbitrator answer and the follow-up record both land there, and it is process-global.
struct HistoryGuard {
    previous: Option<crate::orchestrator::SharedSteeringHistory>,
    current: crate::orchestrator::SharedSteeringHistory,
}

impl HistoryGuard {
    fn install() -> Self {
        let previous = crate::orchestrator::get_steering_history();
        let current: crate::orchestrator::SharedSteeringHistory =
            std::sync::Arc::new(std::sync::RwLock::new(Vec::new()));
        crate::orchestrator::set_steering_history(std::sync::Arc::clone(&current));
        Self { previous, current }
    }

    fn snapshot(&self) -> Vec<(String, String)> {
        self.current.read().unwrap().clone()
    }
}

impl Drop for HistoryGuard {
    fn drop(&mut self) {
        match self.previous.take() {
            Some(previous) => crate::orchestrator::set_steering_history(previous),
            None => crate::orchestrator::set_steering_history(std::sync::Arc::new(
                std::sync::RwLock::new(Vec::new()),
            )),
        }
    }
}

/// (b) The user's steering instruction reaches the worker as exactly one notice, the worker's
/// reply is accepted **as that worker**, exactly one arbitrator turn runs, and its answer is
/// delivered to the user exactly once.
#[tokio::test]
async fn test_notice_reply_round_trip_reaches_the_user_exactly_once() {
    let _notice_lock = notice_lock().await;
    clear_all_notices();
    let history = HistoryGuard::install();

    let server = MockServer::start().await;
    let received_bodies = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
    let rb = std::sync::Arc::clone(&received_bodies);
    let decision = serde_json::json!({
        "decision": "SynthesizeResponse",
        "response": "The refactor is finished; the suite is green.",
        "follow_up_prompt": null,
        "user_status": null
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(move |req: &wiremock::Request| {
            rb.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&req.body).to_string());
            ResponseTemplate::new(200).set_body_string(sse_delta(&decision.to_string()))
        })
        .mount(&server)
        .await;
    let _config = ActiveConfigGuard::with_backend(server.uri());

    // A live worker: the notice is filed under the key this worker drains with, and the reply
    // is attributed through the very token the runner scoped around its turn.
    let token = tokio_util::sync::CancellationToken::new();
    let worker = crate::orchestrator::register_active_worker_with_token(
        Some("t-0520".to_string()),
        "coder".to_string(),
        "refactor the parser".to_string(),
        Some(token.clone()),
    );
    let worker_key = worker.0.clone();

    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::orchestrator::set_event_sender(event_tx);

    // Exactly one notice for this instruction, addressed by the effective key.
    post_notice_to_worker(
        &worker_key,
        "Is the refactor done yet?",
        Some("notice-t0520-round"),
    );
    assert_eq!(
        crate::orchestrator::notice::worker_inbox_len(&worker_key),
        1,
        "the addressed worker must hold exactly one copy of the instruction"
    );

    // What the worker actually receives, and the id it has to echo back.
    let delivered = crate::orchestrator::notice::drain_worker_notices(&worker_key);
    assert_eq!(delivered.len(), 1);
    assert_eq!(delivered[0].notice_id, "notice-t0520-round");

    let invocation = ToolInvocation {
        name: TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-t0520-round",
            "message": "Refactor done, cargo test is green."
        }),
    };
    let res = crate::orchestrator::CURRENT_WORKER_TOKEN
        .scope(
            token.clone(),
            dispatch_for_async(&invocation, ToolCaller::Specialist(Agent::Coder)),
        )
        .await
        .expect("the addressed worker's reply must be accepted");
    assert!(!res.is_error);

    // One arbitrator turn — no duplicate evaluation call.
    assert_eq!(
        received_bodies.lock().unwrap().len(),
        1,
        "one accepted reply must cost exactly one arbitrator turn"
    );

    // The notice is resolved and the reply recorded against it.
    assert!(get_pending_notice("notice-t0520-round").is_none());
    let reply = get_worker_reply("notice-t0520-round").expect("reply must be recorded");
    assert_eq!(reply.worker_tag, worker_key);
    assert_eq!(reply.reply_message, "Refactor done, cargo test is green.");

    // The answer reaches the user **exactly once**.
    let mut answers: Vec<String> = Vec::new();
    while let Ok(event) = event_rx.try_recv() {
        if let crate::ui::Event::SteerResponse(text) = event {
            answers.push(text);
        }
    }
    assert_eq!(
        answers.len(),
        1,
        "the user must get exactly one arbitrator answer, got {answers:?}"
    );
    assert!(answers[0].contains("[Arbitrator]:"));
    assert!(answers[0].contains("The refactor is finished; the suite is green."));

    // …and the steering history holds one resolved entry for the dialogue.
    let entries = history.snapshot();
    assert_eq!(entries.len(), 1, "one dialogue, one entry: {entries:?}");
    assert!(entries[0].1.contains("The refactor is finished"));
    assert!(!entries[0].1.contains("awaiting specialist reply"));

    drop(worker);
}

/// (c) Round two of a dialogue must reach **the worker that was addressed**, and a second worker
/// of the same role must be able neither to receive the follow-up nor to answer the first
/// worker's notice. Before the fix the follow-up was filed under the bare role name (`coder`),
/// which every coder in the process drains.
#[tokio::test]
async fn test_follow_up_reaches_the_addressed_worker_and_not_its_same_role_sibling() {
    let _notice_lock = notice_lock().await;
    clear_all_notices();
    let history = HistoryGuard::install();

    let server = MockServer::start().await;
    let decision = serde_json::json!({
        "decision": "AskFollowUp",
        "response": null,
        "follow_up_prompt": "Which suite did you run?",
        "user_status": null
    });
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse_delta(&decision.to_string())))
        .mount(&server)
        .await;
    let _config = ActiveConfigGuard::with_backend(server.uri());

    // Two live workers of the **same role**.
    let token_a = tokio_util::sync::CancellationToken::new();
    let token_b = tokio_util::sync::CancellationToken::new();
    let worker_a = crate::orchestrator::register_active_worker_with_token(
        Some("t-0521".to_string()),
        "coder".to_string(),
        "first coder".to_string(),
        Some(token_a.clone()),
    );
    let worker_b = crate::orchestrator::register_active_worker_with_token(
        Some("t-0522".to_string()),
        "coder".to_string(),
        "second coder".to_string(),
        Some(token_b.clone()),
    );
    let key_a = worker_a.0.clone();
    let key_b = worker_b.0.clone();
    assert_ne!(key_a, key_b, "two workers must hold distinct keys");

    let (event_tx, _event_rx) = tokio::sync::mpsc::unbounded_channel();
    crate::orchestrator::set_event_sender(event_tx);

    post_notice_to_worker(
        &key_a,
        "Did you land the patch?",
        Some("notice-t0521-dialogue"),
    );

    // The sibling cannot answer it, even though it shares the role name.
    let foreign = ToolInvocation {
        name: TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-t0521-dialogue",
            "message": "answering as the other coder"
        }),
    };
    let rejected = crate::orchestrator::CURRENT_WORKER_TOKEN
        .scope(
            token_b.clone(),
            dispatch_for_async(&foreign, ToolCaller::Specialist(Agent::Coder)),
        )
        .await;
    let detail = match rejected {
        Err(err) => err.to_string(),
        Ok(ok) => panic!(
            "a same-role sibling must not be able to answer another worker's notice, got ok: {:?}",
            ok.content
        ),
    };
    assert!(
        detail.contains("worker-mismatch"),
        "the sibling reply must be rejected as worker-mismatch, got {detail}"
    );
    assert!(get_pending_notice("notice-t0521-dialogue").is_some());

    // The addressed worker answers, and the arbitrator asks a follow-up.
    let mine = ToolInvocation {
        name: TOOL_REPLY_TO_ARBITRATOR.to_string(),
        arguments: serde_json::json!({
            "notice_id": "notice-t0521-dialogue",
            "message": "Landed, and the crate suite is green."
        }),
    };
    crate::orchestrator::CURRENT_WORKER_TOKEN
        .scope(
            token_a.clone(),
            dispatch_for_async(&mine, ToolCaller::Specialist(Agent::Coder)),
        )
        .await
        .expect("the addressed worker's reply must be accepted");

    // The follow-up is queued for the addressed worker **only**.
    let follow_ups = crate::orchestrator::notice::peek_worker_inbox(&key_a);
    assert_eq!(
        follow_ups.len(),
        1,
        "the follow-up must be delivered to the addressed worker, got {follow_ups:?}"
    );
    assert_eq!(follow_ups[0].user_inquiry, "Which suite did you run?");
    assert_eq!(follow_ups[0].notice_id, "notice-t0521-dialogue");
    assert_eq!(follow_ups[0].target_worker, key_a);
    assert!(
        crate::orchestrator::notice::peek_worker_inbox(&key_b).is_empty(),
        "the same-role sibling must never receive the follow-up"
    );

    // And it is recorded as addressed to that key, not to the role name.
    let entries = history.snapshot();
    assert_eq!(entries.len(), 1, "one dialogue, one entry: {entries:?}");
    assert!(
        entries[0].1.contains(&format!("to {key_a}:")) && !entries[0].1.contains(" to coder:"),
        "the follow-up record must name the addressed key, got {}",
        entries[0].1
    );

    drop(worker_a);
    drop(worker_b);
}

// 2. Verdict identity rule at the dispatch layer

fn verdict_invocation() -> ToolInvocation {
    ToolInvocation {
        name: TOOL_LEAVE_VERDICT.to_string(),
        arguments: serde_json::json!({
            "verdict": "APPROVED",
            "comments": "self-certified"
        }),
    }
}

fn assert_forbidden(res: Result<ToolResult, ToolError>, caller: &str) {
    match res {
        Err(ToolError::Forbidden { tool, caller: got }) => {
            assert_eq!(tool, TOOL_LEAVE_VERDICT);
            assert_eq!(got, caller);
        }
        Err(other) => panic!("expected Forbidden for {caller}, got: {other}"),
        Ok(ok) => panic!(
            "{caller} must not be able to record a verdict through dispatch, got ok: {:?}",
            ok.content
        ),
    }
}

#[test]
fn test_dispatch_refuses_leave_verdict_from_a_coder_role() {
    assert_forbidden(
        dispatch_for(&verdict_invocation(), ToolCaller::Specialist(Agent::Coder)),
        "coder",
    );
}

#[test]
fn test_dispatch_refuses_leave_verdict_from_a_wildcard_generalist() {
    // The Generalist namespace is literally `*`, so the allowlist admits this
    // call: only the identity rule can refuse it.
    assert_forbidden(
        dispatch_for(
            &verdict_invocation(),
            ToolCaller::Specialist(Agent::Generalist),
        ),
        "generalist",
    );
}

#[test]
fn test_dispatch_refuses_leave_verdict_admitted_by_an_explicit_allowlist() {
    // A prompt/blueprint allowlist naming the verdict tool is not an identity.
    let caller = ToolCaller::SpecialistWithTools {
        agent: Agent::Coder,
        allowed_tools: vec![TOOL_LEAVE_VERDICT.to_string()],
    };
    assert_forbidden(dispatch_for(&verdict_invocation(), caller), "coder");
}

#[test]
fn test_dispatch_accepts_leave_verdict_from_the_validator() {
    let res = dispatch_for(
        &verdict_invocation(),
        ToolCaller::Specialist(Agent::Validator),
    )
    .expect("the validator must be able to record a verdict");
    assert!(!res.is_error);
    assert!(
        res.content
            .contains("Verdict recorded via leave_verdict: APPROVED")
    );

    let blueprint_caller = ToolCaller::SpecialistWithTools {
        agent: Agent::Validator,
        allowed_tools: vec![TOOL_LEAVE_VERDICT.to_string()],
    };
    let res = dispatch_for(&verdict_invocation(), blueprint_caller)
        .expect("a validator with an explicit allowlist must still be able to record");
    assert!(!res.is_error);
}

#[tokio::test]
async fn test_async_dispatch_applies_the_same_verdict_identity_rule() {
    assert_forbidden(
        dispatch_for_async(&verdict_invocation(), ToolCaller::Specialist(Agent::Coder)).await,
        "coder",
    );
    assert_forbidden(
        dispatch_for_async(
            &verdict_invocation(),
            ToolCaller::Specialist(Agent::Generalist),
        )
        .await,
        "generalist",
    );
    let res = dispatch_for_async(
        &verdict_invocation(),
        ToolCaller::Specialist(Agent::Validator),
    )
    .await
    .expect("the validator must be able to record a verdict");
    assert!(!res.is_error);
    assert!(
        res.content
            .contains("Verdict recorded via leave_verdict: APPROVED")
    );
}

// 3. `read_file` truncation report vs the generic tool-output clip

/// A `read_file` truncation report in the exact shape the reader appends.
fn read_file_report() -> String {
    format!(
        "\n\n{marker} truncated: true\n\
         reason: file_bytes=300000 exceeds read_cap_bytes=262144\n\
         file_bytes: 300000\n\
         read_cap_bytes: 262144\n\
         bytes_read: 262144\n\
         loaded_characters: 262144\n\
         showing_characters: 0-8000\n\
         next_offset: 8000\n\
         hint: re-run with offset=8000 limit=8000 to continue inside the loaded head; bytes past 262144 were never read, so use grep_search/glob to reach sections further in.",
        marker = fs::READ_FILE_TRUNCATION_MARKER,
    )
}

#[tokio::test]
async fn test_read_file_truncation_report_survives_dispatch() {
    let dir = tempfile::tempdir().expect("isolated tempdir root");
    let line = "fn filler_probe_line() -> u32 { 1 } // padded head line for the read cap\n";
    let mut body = String::new();
    while body.len() < 300_000 {
        body.push_str(line);
    }
    body.push_str("SENTINEL_PAST_THE_READ_CEILING");
    std::fs::write(dir.path().join("big.rs"), &body).expect("write the oversized file");

    let invocation = ToolInvocation {
        name: TOOL_READ_FILE.to_string(),
        arguments: serde_json::json!({ "path": "big.rs", "limit": 8_000 }),
    };
    let result = with_workspace_root(dir.path(), async {
        dispatch_for_async(&invocation, ToolCaller::Specialist(Agent::Coder)).await
    })
    .await
    .expect("read_file must not fail on an oversized file");

    assert!(!result.is_error);
    // The read ceiling bit: content past it never reached the model.
    assert!(!result.content.contains("SENTINEL_PAST_THE_READ_CEILING"));
    // …and the reason it bit is still in the payload handed to the model.
    assert!(result.content.contains(fs::READ_FILE_TRUNCATION_MARKER));
    assert!(result.content.contains("truncated: true"));
    assert!(result.content.contains("read_cap_bytes: 262144"));
    assert!(result.content.contains("next_offset: 8000"));
    assert!(result.content.contains("hint: re-run with offset=8000"));
}

#[test]
fn test_output_length_limit_keeps_a_realistic_read_file_report_in_the_tail() {
    // 8 000 multi-byte characters: the clip counts bytes, so it triggers here even
    // though the page is within the character limit, and the (ASCII) report sits
    // in the preserved tail.
    let mut original = ToolResult::ok("é".repeat(8_000));
    original.content.push_str(&read_file_report());
    let before = original.content.len();
    let clipped = apply_tool_output_length_limit(original);
    assert!(
        clipped.content.len() < before,
        "the clip must have triggered for this payload"
    );
    assert!(clipped.content.contains(fs::READ_FILE_TRUNCATION_MARKER));
    assert!(clipped.content.contains("truncated: true"));
    assert!(clipped.content.contains("next_offset: 8000"));
    assert!(clipped.content.contains("hint: re-run with offset=8000"));
}

#[test]
fn test_output_length_limit_widens_the_tail_for_an_oversized_read_file_report() {
    // Structural guarantee: even a report longer than the tail window keeps its
    // header, so the clip can never hide the fact that a read was truncated.
    let oversized_report = format!(
        "{} truncated: true\nnext_offset: 8000\n{}",
        fs::READ_FILE_TRUNCATION_MARKER,
        "x".repeat(4_000)
    );
    let mut original = ToolResult::ok("a".repeat(30_000));
    original.content.push_str(&oversized_report);

    let clipped = apply_tool_output_length_limit(original);
    assert!(clipped.content.contains(fs::READ_FILE_TRUNCATION_MARKER));
    assert!(clipped.content.contains("truncated: true"));
    assert!(clipped.content.contains("next_offset: 8000"));
}

#[test]
fn test_output_length_limit_does_not_widen_the_tail_when_the_report_is_already_in_the_head() {
    // The complement of the previous test: when the report already lives inside the
    // preserved head window, the guard must not widen the tail at all — the head/tail
    // budget of the generic clip stays exactly as it was.
    let payload = {
        let mut text = String::from("lead-in: ");
        text.push_str(&read_file_report());
        text.push_str(&"b".repeat(30_000));
        text
    };
    let full_len = payload.len();
    assert!(full_len > MAX_TOOL_OUTPUT_CHARS);

    let clipped = apply_tool_output_length_limit(ToolResult::ok(payload));
    // The report survives via the head …
    assert!(clipped.content.contains(fs::READ_FILE_TRUNCATION_MARKER));
    assert!(clipped.content.contains("truncated: true"));
    // … and the tail window was not stretched to reach it.
    let omitted = full_len - (7_000 + 2_000);
    assert!(
        clipped
            .content
            .contains(&format!("[... TRUNCATED {omitted} CHARACTERS"))
    );
}
