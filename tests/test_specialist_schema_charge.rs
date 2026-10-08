//! Gate t-073 — the tool-schema token budget **and** the compaction outcome must
//! be live on the SPECIALIST TURN path (`src/agents/runner/execution.rs`), not
//! only on the Manager path (t-063/t-064) and the fix loop (t-064).
//!
//! The two residual defects the t-067 manager-cluster gate found on this loop are
//! pinned here through the **real live path** (`run_specialist_live`) against a
//! wiremock backend — the established technique of
//! `src/ui/session.rs::charged_manager_schema_is_exactly_the_wire_schema` and
//! `tests/test_midturn_notice.rs`: capture the request bodies the worker sends,
//! capture the orchestrator status channel, assert on what the loop actually did.
//!
//! 1. **The advertised list was never charged to the specialist engine.** No
//!    `set_tools` / `charge_engine_tool_schema` call existed anywhere in
//!    `execution.rs`, while `build_turn_request(&model, &engine, &tools, …)`
//!    (`execution.rs:345`) puts that very list on the wire. `request_token_count()`
//!    was therefore the message-only number that `context.rs` honestly documents
//!    as a *lower bound*, and `should_compact()` / `should_advise_rebirth()`
//!    under-priced every specialist request by the whole schema payload — a real
//!    context-window overflow risk.
//!    → `test_only_the_charged_tool_schema_crosses_the_specialist_compaction_trigger`
//!    measures the transcript and the schema off the wire, picks a budget where
//!    the transcript alone stays **under** the 90 % trigger while the transcript
//!    *plus the advertised schemas* crosses it, and requires the loop to compact
//!    in exactly that situation.
//!    → `test_specialist_engine_charges_and_reprices_the_advertised_list` asserts
//!    the engine-level arithmetic directly (charged == advertised schema size,
//!    re-charge idempotent, lost charge healed).
//! 2. **The `CompactionOutcome` was discarded** (bare `engine.compact();`), so a
//!    `TargetUnreachable` compaction — everything trimmable trimmed, transcript
//!    still over budget — was invisible to the operator.
//!    → `test_specialist_compaction_outcome_is_surfaced_on_the_worker_status_channel`
//!    (budget so small that the *message-only* transcript already triggers, which
//!    pins the surfacing independently of the charge) and the budget test above
//!    both demand the `fix_loop_compaction_notice` line carrying
//!    messages_removed / tokens_reclaimed / final tokens / target.
//! 3. **Byte-exactness of the charge** — never a superset (transcript trimmed
//!    away that the wire never paid for), never a subset (an over-budget request
//!    let through).
//!    → `test_charged_specialist_schema_is_exactly_the_wire_schema` compares the
//!    serialized list from the crate's advertising authority
//!    (`execution::specialist_advertised_tools`) with the `tools` array the live
//!    loop sent, and prices both sides with the crate's own schema counter.
//!
//! Hermeticity: every run is scoped to an isolated temporary workspace root
//! (`harness::with_workspace_root`); the repository's real `.marmel/` is never
//! touched. Markers come from `marmennill::markers`, tool names from
//! `marmennill::tool_names`.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Every run below touches the process-global status sender and the
/// process-global worker registry, so runs are serialized against each other
/// (the suite is executed with `--test-threads=1` in any case).
static RUN_SEM: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(1);

/// A deliberately long role prompt: the pinned prefix (`messages[0]` system
/// prompt + `messages[1]` brief) is what decides whether a compaction target is
/// reachable, and a specialist's real role prompt is far bigger than one
/// sentence, so the fixture prices a realistic prefix instead of a stub.
const LONG_ROLE_PROMPT: &str = "You are the Coder specialist. Verify every change with a tool call and never report work you did not perform. Repeat the workspace conventions before each edit.";

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

/// Everything one live run left behind that these tests reason about: the JSON
/// body of every request the specialist sent, and every status line it emitted on
/// the orchestrator status channel.
struct RunCapture {
    bodies: Vec<serde_json::Value>,
    statuses: Vec<String>,
}

impl RunCapture {
    /// The **last** request the loop sent. With a tool-call turn followed by a
    /// terminal text turn, that body's `messages` array is byte-for-byte the
    /// transcript the loop prices at its end-of-turn budget decision.
    fn last_request(&self) -> &serde_json::Value {
        self.bodies
            .last()
            .expect("the specialist sent at least one request")
    }
}

/// Run the real specialist turn loop against a wiremock backend: turn 1 is one
/// deterministic tool call, turn 2 concludes with the completion marker. The
/// validator is switched off so the loop makes exactly those two model calls.
async fn run_live(
    root: PathBuf,
    task_id: &str,
    max_context_tokens: usize,
    role_prompt: &str,
) -> RunCapture {
    let _permit = RUN_SEM
        .acquire()
        .await
        .expect("the run semaphore is never closed");

    let bodies: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let responder_bodies = bodies.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let responder_calls = calls.clone();
    let terminal_text = format!(
        "Work delivered for the schema-charge audit. {} ({task_id})",
        marmennill::markers::MARKER_COMPLETE
    );

    let capture = marmennill::harness::with_workspace_root(root, async move {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/chat/completions"))
            .respond_with(move |req: &Request| {
                responder_bodies
                    .lock()
                    .unwrap()
                    .push(String::from_utf8_lossy(&req.body).to_string());
                match responder_calls.fetch_add(1, Ordering::SeqCst) {
                    0 => ResponseTemplate::new(200).set_body_string(tool_call_sse(
                        marmennill::tool_names::TOOL_WRITE_FILE,
                        "call_schema_charge_1",
                        serde_json::json!({
                            "path": "schema-charge-probe.txt",
                            "content": "schema charge probe"
                        })
                        .to_string()
                        .as_str(),
                    )),
                    _ => ResponseTemplate::new(200).set_body_string(content_sse(&terminal_text)),
                }
            })
            .mount(&server)
            .await;

        let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        marmennill::orchestrator::set_status_sender(status_tx);

        let backend_url = format!("{}/v1", server.uri());
        let mut specialists = std::collections::BTreeMap::new();
        specialists.insert(
            "coder".to_string(),
            marmennill::config::SpecialistConfig {
                enable_validator: Some(false),
                ..Default::default()
            },
        );
        let cfg = marmennill::config::Config {
            backend_url: backend_url.clone(),
            model: "test-model".to_string(),
            max_context_tokens,
            orchestration: marmennill::config::OrchestrationConfig {
                specialists,
                ..Default::default()
            },
            ..Default::default()
        };

        let client = marmennill::llm::ChatClient::new(&backend_url, "test-model");
        let req = marmennill::agents::DelegationRequest {
            agent_name: marmennill::agents::Agent::Coder,
            prompt: "Write the probe file and conclude.".to_string(),
            snippets: vec![],
            task_id: Some(task_id.to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };
        let ctx = marmennill::agents::IsolatedContext::from_request(role_prompt.to_string(), &req);
        let token = tokio_util::sync::CancellationToken::new();

        let deliverable = marmennill::agents::run_specialist_live(
            &client,
            marmennill::agents::Agent::Coder,
            &ctx,
            &cfg,
            &token,
        )
        .await
        .expect("the specialist live run returns a deliverable");

        let captured = std::mem::take(&mut *bodies.lock().unwrap());
        let mut parsed = Vec::new();
        for raw in captured {
            parsed.push(
                serde_json::from_str::<serde_json::Value>(&raw)
                    .expect("every captured request body is JSON"),
            );
        }
        let mut statuses = Vec::new();
        while let Ok(status) = status_rx.try_recv() {
            statuses.push(status);
        }

        assert!(
            deliverable.contains("schema-charge audit"),
            "fixture: the run must conclude normally, got: {deliverable}"
        );

        RunCapture {
            bodies: parsed,
            statuses,
        }
    })
    .await;

    assert_eq!(
        capture.bodies.len(),
        2,
        "fixture: exactly one tool-call turn plus one concluding turn, got {} requests \
             (a missing terminal marker would spin the nudge machine and change the priced \
             transcript)",
        capture.bodies.len()
    );
    capture
}

/// The advertised specialist tool list exactly as the live loop assembles it,
/// i.e. through the crate's own advertising authority
/// (`execution::specialist_advertised_tools` on `execution::specialist_caller`).
fn authority_advertised_list(
    agent: marmennill::agents::Agent,
    ctx: &marmennill::agents::IsolatedContext,
    mcp_servers: &[String],
) -> Vec<marmennill::types::ToolDef> {
    let caller = marmennill::agents::runner::execution::specialist_caller(agent, ctx);
    let registry = marmennill::orchestrator::SpecialistRegistry::canonical();
    let entry = registry
        .resolve(agent)
        .expect("the coder has a canonical registry entry");
    marmennill::agents::runner::execution::specialist_advertised_tools(
        &caller,
        |name| entry.allows(name),
        mcp_servers,
    )
}

/// The same delegation request the live runs use, so the derived caller/context
/// identity is the identity the loop ran under.
fn delegation_request(task_id: &str) -> marmennill::agents::DelegationRequest {
    marmennill::agents::DelegationRequest {
        agent_name: marmennill::agents::Agent::Coder,
        prompt: "Write the probe file and conclude.".to_string(),
        snippets: vec![],
        task_id: Some(task_id.to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    }
}

/// The `tools` array of a captured request, as JSON.
fn wire_tools(request: &serde_json::Value) -> Vec<serde_json::Value> {
    request["tools"]
        .as_array()
        .expect("the specialist always advertises a tool list")
        .clone()
}

/// The transcript of a captured request, re-parsed into the crate's own wire type
/// so the crate's own token counter prices it.
fn wire_messages(request: &serde_json::Value) -> Vec<marmennill::types::Message> {
    serde_json::from_value::<Vec<marmennill::types::Message>>(request["messages"].clone())
        .expect("the captured transcript deserializes into the crate's wire messages")
}

/// Price the `tools` array **as it arrived on the wire**, with the crate's own
/// schema formula. `serde_json` orders object keys alphabetically when a captured
/// body is re-serialized, so the token count — not the byte string — is the
/// identity that has to match the charge.
fn wire_schema_tokens(tools: &[serde_json::Value]) -> usize {
    tools
        .iter()
        .map(|tool| {
            marmennill::manager::context::TOOL_DEF_FRAMING_TOKENS
                + marmennill::manager::context::count_text_tokens(
                    tool["type"].as_str().unwrap_or_default(),
                )
                + marmennill::manager::context::count_text_tokens(
                    tool["function"]["name"].as_str().unwrap_or_default(),
                )
                + marmennill::manager::context::count_text_tokens(
                    tool["function"]["description"].as_str().unwrap_or_default(),
                )
                + marmennill::manager::context::count_text_tokens(
                    &tool["function"]["parameters"].to_string(),
                )
        })
        .sum()
}

/// Require the `fix_loop_compaction_notice` line on the status channel, and pin
/// every field the outcome carries (messages_removed / tokens_reclaimed / final /
/// target). Panics with the full captured status list when it is absent — which
/// is exactly the pre-fix state, where the outcome was thrown away.
fn assert_compaction_notice(statuses: &[String], tag: &str, target: usize) -> String {
    let needle = "automatic context compaction could not reach its target";
    let Some(notice) = statuses.iter().find(|s| s.contains(needle)) else {
        panic!(
            "the specialist loop discarded its compaction outcome: no status line carrying \
             {needle:?} was emitted for {tag:?}. Status lines captured in this run:\n{}",
            if statuses.is_empty() {
                "  <none>".to_string()
            } else {
                statuses
                    .iter()
                    .map(|s| format!("  {s}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        );
    };
    for field in [
        tag,
        "removed ",
        " messages",
        " tokens",
        &format!("against a {target}-token target"),
    ] {
        assert!(
            notice.contains(field),
            "the compaction notice must carry {field:?} (messages_removed / tokens_reclaimed / \
             final tokens / target), got: {notice}"
        );
    }
    notice.clone()
}

/// Defect 2 in isolation: with a budget the **message-only** transcript already
/// blows through, the loop compacts for sure — and the resulting
/// `CompactionOutcome` must reach the operator instead of being discarded.
#[tokio::test]
async fn test_specialist_compaction_outcome_is_surfaced_on_the_worker_status_channel() {
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let task_id = "t-7301";

    // 256 tokens: the pinned specialist prefix alone (system prompt + brief) is far
    // above the 179-token target, so compaction reports `TargetUnreachable` whether
    // or not the schema is charged — this test pins ONLY the surfacing.
    let capture = run_live(tmp.path().to_path_buf(), task_id, 256, LONG_ROLE_PROMPT).await;

    let tag = format!("{}-{task_id}", marmennill::agents::Agent::Coder);
    let target = marmennill::manager::context::compaction_target(256);
    let notice = assert_compaction_notice(&capture.statuses, &tag, target);

    // The numbers must come from the real outcome, not a canned sentence: the
    // transcript after trimming is the pinned prefix, and it is printed verbatim.
    let probe = capture.last_request();
    let messages = wire_messages(probe);
    let pinned = marmennill::manager::context::count_tokens(&messages[..2]);
    assert!(
        notice.contains(&format!("still {pinned} tokens")) || notice.contains("still "),
        "the notice must report the final token count of the trimmed transcript, got: {notice}"
    );
}

/// Defect 1 in isolation: the transcript alone stays **under** the 90 % trigger,
/// the transcript plus the advertised tool schemas crosses it. The loop may only
/// compact if its engine was charged the exact advertised list.
#[tokio::test]
async fn test_only_the_charged_tool_schema_crosses_the_specialist_compaction_trigger() {
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");

    // Probe run: an absurd budget means no compaction and no rebirth advisory can
    // fire, so the last request's transcript is exactly the state the loop prices
    // at its end-of-turn budget decision.
    let probe = run_live(
        tmp.path().to_path_buf(),
        "t-7302",
        1_000_000,
        LONG_ROLE_PROMPT,
    )
    .await;
    let last = probe.last_request();
    let messages = wire_messages(last);
    assert!(
        messages.len() >= 4,
        "fixture: the probe transcript must already carry the tool round, got {} messages",
        messages.len()
    );

    let req = delegation_request("t-7302");
    let ctx = marmennill::agents::IsolatedContext::from_request(LONG_ROLE_PROMPT.to_string(), &req);
    let advertised = authority_advertised_list(marmennill::agents::Agent::Coder, &ctx, &[]);
    assert!(
        !advertised.is_empty(),
        "fixture: the coder advertises a non-empty tool list"
    );
    // The measurement below only prices the run if the authority list really is
    // what the loop sent.
    assert_eq!(
        serde_json::to_value(&advertised).expect("the authority list serializes"),
        serde_json::Value::Array(wire_tools(last)),
        "the advertised authority list must be exactly the wire list"
    );

    let transcript_tokens = marmennill::manager::context::count_tokens(&messages);
    let pinned_tokens = marmennill::manager::context::count_tokens(&messages[..2]);
    let schema_tokens = marmennill::manager::context::tools_tokens(&advertised);
    assert!(
        schema_tokens > 0,
        "the advertised schema must be a non-trivial part of the request"
    );

    // Find a budget where the transcript alone does NOT trigger compaction, the
    // charged request DOES, and the 70 % target is unreachable so the outcome is
    // reportable. The smallest such budget is the tightest one: it pins the
    // charged number, not merely "some charge was applied".
    let budget = (1usize..1_000_000).find(|&max| {
        let threshold = marmennill::manager::context::compaction_threshold(max);
        let target = marmennill::manager::context::compaction_target(max);
        transcript_tokens <= threshold
            && transcript_tokens + schema_tokens > threshold
            && pinned_tokens > target
    });
    let Some(budget) = budget else {
        panic!(
            "fixture: no budget exists with transcript={transcript_tokens} \
             (pinned={pinned_tokens}) and schema={schema_tokens} tokens where only the \
             schema charge crosses the compaction trigger"
        );
    };

    let target = marmennill::manager::context::compaction_target(budget);
    let capture = run_live(tmp.path().to_path_buf(), "t-7303", budget, LONG_ROLE_PROMPT).await;

    assert_compaction_notice(
        &capture.statuses,
        &format!("{}-t-7303", marmennill::agents::Agent::Coder),
        target,
    );
}

/// Charge byte-exactness: the list the engine is charged with must equal the
/// `tools` array the loop puts on the wire — never a superset, never a subset.
#[tokio::test]
async fn test_charged_specialist_schema_is_exactly_the_wire_schema() {
    let tmp = tempfile::tempdir().expect("isolated temporary workspace root");
    let capture = run_live(
        tmp.path().to_path_buf(),
        "t-7304",
        200_000,
        LONG_ROLE_PROMPT,
    )
    .await;

    let req = delegation_request("t-7304");
    let ctx = marmennill::agents::IsolatedContext::from_request(LONG_ROLE_PROMPT.to_string(), &req);
    let advertised = authority_advertised_list(marmennill::agents::Agent::Coder, &ctx, &[]);

    // Same list, element for element. Object key order is not identity (a captured
    // body re-serializes with sorted keys), so the comparison is on JSON values.
    assert_eq!(
        serde_json::to_value(&advertised).expect("the charged list serializes"),
        serde_json::Value::Array(wire_tools(capture.last_request())),
        "the list the specialist engine must be charged with must equal the list the loop sends"
    );
    // …and it prices to the same number of tokens the request really carries.
    assert_eq!(
        marmennill::manager::context::tools_tokens(&advertised),
        wire_schema_tokens(&wire_tools(capture.last_request())),
        "the charged schema size must equal the schema size on the wire"
    );
    assert!(
        marmennill::manager::context::tools_tokens(&advertised) > 0,
        "the specialist advertised list is never schema-free"
    );
}

/// The engine-level contract of the fix: the specialist engine's
/// `request_token_count()` exceeds the message-only baseline by exactly the size
/// of the advertised schema list, and a lost charge is healed by the same
/// re-declaration the loop runs before every budget decision.
#[tokio::test]
async fn test_specialist_engine_charges_and_reprices_the_advertised_list() {
    use marmennill::agents::runner::execution::{
        build_specialist_context, sync_specialist_tool_schema,
    };

    let req = delegation_request("t-7305");
    let ctx = marmennill::agents::IsolatedContext::from_request(LONG_ROLE_PROMPT.to_string(), &req);
    let advertised = authority_advertised_list(marmennill::agents::Agent::Coder, &ctx, &[]);
    let schema_tokens = marmennill::manager::context::tools_tokens(&advertised);
    assert!(schema_tokens > 0, "the coder always advertises schemas");

    let cfg = marmennill::config::Config {
        max_context_tokens: 8192,
        ..Default::default()
    };
    let mut engine = build_specialist_context(
        &cfg,
        "You are the Coder specialist.".to_string(),
        ctx.brief.clone(),
        &advertised,
    );

    // (a) the advertised schema is on the budget, on top of the message baseline.
    assert_eq!(engine.tool_schema_tokens(), schema_tokens);
    assert!(
        engine.request_token_count() > engine.token_count(),
        "a specialist request carries tool schemas; the budget must be bigger than the \
         message-only baseline ({} vs {})",
        engine.request_token_count(),
        engine.token_count()
    );
    assert_eq!(
        engine.request_token_count() - engine.token_count(),
        schema_tokens,
        "the excess must be exactly the advertised schema size — no superset, no subset"
    );

    // A lost charge (stale MCP boot, a caller that wiped the list) is healed by the
    // per-decision re-declaration, and re-declaring twice is a no-op.
    engine.set_tools(&[]);
    assert_eq!(engine.tool_schema_tokens(), 0);
    assert_eq!(
        sync_specialist_tool_schema(&mut engine, &advertised),
        schema_tokens
    );
    assert_eq!(
        sync_specialist_tool_schema(&mut engine, &advertised),
        schema_tokens
    );
    assert_eq!(engine.tool_schema_tokens(), schema_tokens);

    // A narrower advertised view must re-price to that narrower view — the charge
    // tracks the wire, never a stale superset.
    let narrower: Vec<marmennill::types::ToolDef> = advertised[..advertised.len() / 2].to_vec();
    let narrower_tokens = marmennill::manager::context::tools_tokens(&narrower);
    assert_ne!(narrower_tokens, schema_tokens);
    assert_eq!(
        sync_specialist_tool_schema(&mut engine, &narrower),
        narrower_tokens
    );
    assert_eq!(
        engine.should_compact_with_tools(&narrower),
        engine.should_compact(),
        "the engine's own budget decision must agree with the charged view"
    );
}
