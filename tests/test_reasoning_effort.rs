//! Delegation-path integration coverage for the per-role `reasoning_effort` dial
//! (feature.md): the top-level `reasoning_effort`, the per-role
//! `[orchestration.specialists.<role>] reasoning_effort`, and the per-validator
//! `[orchestration.specialists.<role>] validator_reasoning_effort`.
//!
//! Seam. Every case starts from a throwaway `marmel.toml` **on disk** loaded
//! through the production seam [`marmennill::config::load`], runs a real turn
//! against a real `wiremock` socket, and asserts against the **request body that
//! actually arrived** (`received_requests()[…].body`) — never a re-serialization
//! of the input. That idiom is the one established by
//! `tests/test_rebirth_system_role.rs:150-173`; the round is driven the way
//! `tests/test_validation_loop.rs` and `tests/test_specialist_stream.rs` drive a
//! specialist/validator round (temp workspace root + mock backend).
//!
//! Which link of each chain each test covers:
//!
//! * Manager turn — `Config::reasoning_effort` (`src/config.rs:198`) →
//!   `StreamConfig::from_config` (`src/llm/stream.rs:162-185`, field read at
//!   `:183`) → `chat_client_turn` → `build_request` (`src/llm/stream.rs:470-495`,
//!   applied at `:492`, blank-normalizer at `:457-468`) → wire.
//!   Covered by [`manager_turn_sends_top_level_reasoning_effort`] and
//!   [`nothing_set_sends_no_reasoning_effort_key_anywhere`]. `build_request` is
//!   `pub(crate)`, so the integration-level way to reach it is exactly the pair
//!   the Manager itself uses — `StreamConfig::from_config(&cfg)` plus
//!   `chat_client_turn(…)` (`src/ui/session.rs:1821` and `:2161`) — which is what
//!   these tests do. Nothing is dropped: the whole Manager link is live.
//! * Specialist turn — `SpecialistConfig::reasoning_effort` (`src/config.rs:145`)
//!   resolved over the top-level value in `src/agents/runner/execution.rs:165-177`
//!   → `fix_loop::build_turn_request` (`src/agents/runner/fix_loop.rs:270-290`,
//!   applied at `:284`) → wire. Driven by `run_specialist_live`, the entry point
//!   the delegation path itself calls (`src/agents/runner/mod.rs:96`).
//!   Covered by [`specialist_role_override_reaches_the_wire_and_the_manager_keeps_the_top_level_value`],
//!   [`role_without_an_override_falls_back_to_the_top_level_value`] and
//!   [`blank_reasoning_effort_counts_as_unset_and_falls_through_the_chain`].
//! * Validator pass — `SpecialistConfig::validator_reasoning_effort`
//!   (`src/config.rs:130`) resolved by `resolve_validator_backend`
//!   (`src/agents/runner/fix_loop.rs:569-606`, effort chain at `:602-604`:
//!   validator value → top level, **never** the audited specialist's own value) →
//!   `LoopParams::reasoning_effort` (`src/agents/validation.rs:393`, field
//!   `src/agents/runner/fix_loop.rs:244`) → `run_fix_loop` →
//!   `build_turn_request` (`src/agents/runner/fix_loop.rs:864-871`) → wire.
//!   Covered by [`validator_pass_sends_the_validator_value_and_never_the_specialist_value`]
//!   and [`blank_reasoning_effort_counts_as_unset_and_falls_through_the_chain`].
//!
//! Roles are told apart by the `model` each turn advertises: the Manager sends
//! `cfg.model`, the specialist sends its own `[specialists.<role>] model`, and the
//! validator sends the `validator_model` resolved next to the effort value — so
//! one mock backend, one capture list, three unambiguous body classes.
//!
//! Offline and deterministic: no network beyond the loopback wiremock socket, one
//! validator pass per round (`max_validator_iterations = 1`), no wall-clock
//! sensitive assertions.

use marmennill::agents::{Agent, IsolatedContext, run_specialist_live};
use marmennill::config::Config;
use marmennill::config::load as load_config;
use marmennill::llm::{ChatClient, StreamConfig, VecSink, chat_client_turn};
use marmennill::types::Message;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// Model advertised by the Manager turn (`Config::model`).
const MANAGER_MODEL: &str = "manager-model";
/// Model advertised by the coder specialist's own turns.
const CODER_MODEL: &str = "coder-model";
/// Model advertised by the coder's validator pass (`validator_model`, i.e. the
/// key that sits next to `validator_reasoning_effort`).
const VALIDATOR_MODEL: &str = "validator-model";
/// Task id carried by the delegated round.
const TASK_ID: &str = "t-eff01";

/// A `[orchestration.specialists.researcher]` decoy for the fallback case: a
/// **different** role's effort values, on a role that is never delegated to. If a
/// value here ever reached the wire, per-role isolation would be broken.
const RESEARCHER_DECOY: &str = "[orchestration.specialists.researcher]\n\
                                reasoning_effort = \"ultra\"\n\
                                validator_reasoning_effort = \"maximal\"\n";

// ------------------------------------------------------------------ wire fixtures

/// One canned SSE completion frame carrying plain assistant content, no tool call.
fn text_sse(text: &str) -> String {
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-effort",
            "choices": [{ "delta": { "content": text }, "finish_reason": "stop" }]
        })
    )
}

/// One canned SSE frame carrying a `leave_verdict` tool call — the only thing
/// that ends a validator pass with a real verdict.
fn verdict_sse(verdict: &str, comments: &str) -> String {
    let args = serde_json::json!({ "verdict": verdict, "comments": comments }).to_string();
    format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-effort-verdict",
            "choices": [{
                "delta": {
                    "content": null,
                    "tool_calls": [{
                        "index": 0,
                        "id": "call_verdict_1",
                        "type": "function",
                        "function": { "name": "leave_verdict", "arguments": args }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
    )
}

/// A mock backend that answers strictly by the `model` field of the body that
/// arrived, so a Manager turn, a specialist turn and a validator pass each get
/// the reply their own role would get — while `received_requests()` keeps every
/// body for the assertions.
async fn role_routed_backend() -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/chat/completions"))
        .respond_with(|req: &Request| {
            let body: serde_json::Value =
                serde_json::from_slice(&req.body).expect("Marmel must send a JSON request body");
            let model = body["model"].as_str().unwrap_or_default();
            let sse = match model {
                VALIDATOR_MODEL => {
                    verdict_sse("APPROVED", "deliverable verified against the brief")
                }
                MANAGER_MODEL => text_sse("Manager turn acknowledged."),
                _ => text_sse(&format!("Work complete.\n\nMISSION COMPLETE ({TASK_ID})")),
            };
            ResponseTemplate::new(200).set_body_string(sse)
        })
        .mount(&server)
        .await;
    server
}

// ------------------------------------------------------------- config from disk

/// A `[orchestration.specialists.coder]` section with the pinned role models plus
/// the caller's effort lines (`extra` is empty when the role sets no override).
fn coder_section(extra: &str) -> String {
    format!(
        "[orchestration.specialists.coder]\n\
         model = \"{CODER_MODEL}\"\n\
         validator_model = \"{VALIDATOR_MODEL}\"\n\
         enable_validator = true\n\
         max_validator_iterations = 1\n\
         {extra}"
    )
}

/// Write the case's `marmel.toml` and load it through the production config seam,
/// so every case really exercises TOML on disk → `Config` → wire.
///
/// `top_level` is the top-level effort line (empty ⇒ the key is absent) and
/// `sections` the `[orchestration.specialists.*]` sections. The transport
/// identity (`backend_url`, `model`) is pinned **after** `load()` on purpose:
/// `load()` lets `MARMEL_BACKEND_URL` / `MARMEL_MODEL` beat the file
/// (`src/config.rs:389-398`), and an ambient value in a developer's or CI shell
/// must never redirect a test off its own mock backend. No `reasoning_effort`
/// value is touched here — every one of them comes from the file, which is what
/// these tests are about.
fn load_case_config(dir: &TempDir, backend_url: &str, top_level: &str, sections: &str) -> Config {
    let path = dir.path().join("marmel.toml");
    let toml = format!(
        "backend_url = \"{backend_url}\"\nmodel = \"{MANAGER_MODEL}\"\n{top_level}\n{sections}"
    );
    std::fs::write(&path, toml).expect("temp marmel.toml must be writable");

    let mut cfg = load_config(Some(path.to_str().expect("temp path must be utf-8")))
        .expect("the throwaway marmel.toml must load");
    cfg.backend_url = backend_url.to_string();
    cfg.model = MANAGER_MODEL.to_string();
    cfg
}

// ---------------------------------------------------------------- turn drivers

/// Drive one **Manager turn** through the Manager's own production chain:
/// `Config` → `StreamConfig::from_config` → `chat_client_turn` → `build_request`
/// → wire. This is the identical pair the Manager uses in `src/ui/session.rs`
/// (`:1821` builds the `StreamConfig`, `:2161` runs the turn).
async fn drive_manager_turn(server: &MockServer, cfg: &Config) {
    let client = ChatClient::new(format!("{}/v1", server.uri()), MANAGER_MODEL);
    let stream_cfg = StreamConfig::from_config(cfg);
    let mut sink = VecSink::default();
    chat_client_turn(
        &client,
        vec![
            Message::System {
                content: "Manager system prompt.".to_string(),
            },
            Message::User {
                content: "Report the status of the workspace.".to_string(),
            },
        ],
        &stream_cfg,
        &mut sink,
    )
    .await
    .expect("the Manager turn must stream against the mock backend");
    assert!(
        sink.content().contains("Manager turn acknowledged"),
        "the Manager turn must have consumed the streamed reply, got {:?}",
        sink.events
    );
}

/// Drive one **delegation round** for `agent`: the specialist's own turn(s) plus
/// the validator pass that follows, through `run_specialist_live` — the entry
/// point the delegation path itself calls (`src/agents/runner/mod.rs:96`).
async fn drive_delegation_round(server: &MockServer, cfg: &Config, agent: Agent) {
    // Hermeticity: an isolated temporary workspace root, exactly as
    // `tests/test_specialist_stream.rs` does, so nothing resolves against the
    // repository checkout (prompts dir, verdict files, worker journals).
    let workspace = tempfile::tempdir().expect("isolated temporary workspace root");
    let client = ChatClient::new(format!("{}/v1", server.uri()), MANAGER_MODEL);
    let ctx = IsolatedContext {
        role_system_prompt: format!(
            "You are the {} specialist — reasoning-effort integration fixture.",
            agent.as_str()
        ),
        brief: "Produce a one-line deliverable and conclude.".to_string(),
        snippets: Vec::new(),
        task_id: Some(TASK_ID.to_string()),
        image_urls: Vec::new(),
        audio_urls: Vec::new(),
        blueprint: None,
    };
    let token = CancellationToken::new();

    let deliverable =
        marmennill::harness::with_workspace_root(workspace.path().to_path_buf(), async {
            run_specialist_live(&client, agent, &ctx, cfg, &token)
                .await
                .expect("the delegated round must complete")
        })
        .await;

    assert!(
        deliverable.contains("MISSION COMPLETE"),
        "the delegated round must end in an approved deliverable, got: {deliverable}"
    );
}

// ---------------------------------------------------------- body classification

/// Every request body the mock backend actually received, in arrival order.
async fn captured_bodies(server: &MockServer) -> Vec<serde_json::Value> {
    let requests = server
        .received_requests()
        .await
        .expect("wiremock exposes the requests it received");
    requests
        .iter()
        .map(|req| {
            serde_json::from_slice(&req.body).expect("every body Marmel sends must be valid JSON")
        })
        .collect()
}

/// The bodies sent by one model name, i.e. the turns owned by one role.
fn bodies_from(bodies: &[serde_json::Value], model: &str) -> Vec<serde_json::Value> {
    bodies
        .iter()
        .filter(|body| body["model"].as_str() == Some(model))
        .cloned()
        .collect()
}

/// The `reasoning_effort` value carried by a captured body; `None` ⇒ the key is
/// absent from that body.
fn wire_effort(body: &serde_json::Value) -> Option<&str> {
    body.get("reasoning_effort").map(|value| {
        value
            .as_str()
            .expect("`reasoning_effort` must serialize as a JSON string")
    })
}

/// Assert the role behind `model` sent at least one body and that **every** one
/// of them carries exactly `expected` as `reasoning_effort`.
fn assert_role_sends(body_model: &str, bodies: &[serde_json::Value], expected: &str) {
    let role_bodies = bodies_from(bodies, body_model);
    assert!(
        !role_bodies.is_empty(),
        "no request body was captured for the `{body_model}` turn"
    );
    for body in &role_bodies {
        assert_eq!(
            wire_effort(body),
            Some(expected),
            "the `{body_model}` turn must send `\"reasoning_effort\": \"{expected}\"`, got body: {body}"
        );
    }
}

/// Assert the role behind `model` sent at least one body and that **none** of
/// them carries a `reasoning_effort` key at all.
fn assert_role_omits(body_model: &str, bodies: &[serde_json::Value]) {
    let role_bodies = bodies_from(bodies, body_model);
    assert!(
        !role_bodies.is_empty(),
        "no request body was captured for the `{body_model}` turn"
    );
    for body in &role_bodies {
        assert!(
            body.get("reasoning_effort").is_none(),
            "an unset dial must be omitted from the body entirely, got: {body}"
        );
    }
}

// ----------------------------------------------------------------------- cases

/// (1) Top-level `reasoning_effort = "medium"`, no role overrides ⇒ the **Manager
/// turn** body carries `"reasoning_effort":"medium"`.
///
/// Chain: `marmel.toml` → `Config::reasoning_effort` → `StreamConfig::from_config`
/// → `chat_client_turn` → `build_request` → wire.
#[tokio::test]
async fn manager_turn_sends_top_level_reasoning_effort() {
    let dir = tempfile::tempdir().expect("tempdir for the throwaway marmel.toml");
    let server = role_routed_backend().await;
    let backend = format!("{}/v1", server.uri());

    let cfg = load_case_config(&dir, &backend, "reasoning_effort = \"medium\"\n", "");
    assert_eq!(
        cfg.reasoning_effort.as_deref(),
        Some("medium"),
        "`reasoning_effort = \"medium\"` must parse into `Config::reasoning_effort`"
    );

    drive_manager_turn(&server, &cfg).await;

    let bodies = captured_bodies(&server).await;
    assert_eq!(
        bodies.len(),
        1,
        "one Manager turn sends exactly one request body, got {}",
        bodies.len()
    );
    assert_eq!(
        bodies[0]["model"].as_str(),
        Some(MANAGER_MODEL),
        "the captured body is the Manager turn"
    );
    assert_role_sends(MANAGER_MODEL, &bodies, "medium");
}

/// (2) `[orchestration.specialists.coder] reasoning_effort = "high"` ⇒ the coder
/// specialist's request body carries `"high"` **while the Manager body still
/// carries the top-level `"medium"`** — a role override never travels upstream.
#[tokio::test]
async fn specialist_role_override_reaches_the_wire_and_the_manager_keeps_the_top_level_value() {
    let dir = tempfile::tempdir().expect("tempdir for the throwaway marmel.toml");
    let server = role_routed_backend().await;
    let backend = format!("{}/v1", server.uri());

    let cfg = load_case_config(
        &dir,
        &backend,
        "reasoning_effort = \"medium\"\n",
        &coder_section("reasoning_effort = \"high\"\n"),
    );
    let coder = cfg
        .orchestration
        .specialists
        .get("coder")
        .expect("the coder section must be present");
    assert_eq!(
        coder.reasoning_effort.as_deref(),
        Some("high"),
        "`reasoning_effort` must parse into `SpecialistConfig::reasoning_effort`"
    );

    drive_manager_turn(&server, &cfg).await;
    drive_delegation_round(&server, &cfg, Agent::Coder).await;

    let bodies = captured_bodies(&server).await;
    assert_role_sends(CODER_MODEL, &bodies, "high");
    assert_role_sends(MANAGER_MODEL, &bodies, "medium");
    // This role sets no `validator_reasoning_effort`, so its validator pass falls
    // through to the top level — and never inherits the coder's `"high"`.
    assert_role_sends(VALIDATOR_MODEL, &bodies, "medium");
}

/// (3) `[orchestration.specialists.coder] validator_reasoning_effort = "low"` ⇒
/// the **validator-pass** body carries `"low"` and the coder body keeps **its own**
/// `"high"`. Two different values in the same round is the proof that a validator
/// pass never inherits the audited specialist's dial.
#[tokio::test]
async fn validator_pass_sends_the_validator_value_and_never_the_specialist_value() {
    let dir = tempfile::tempdir().expect("tempdir for the throwaway marmel.toml");
    let server = role_routed_backend().await;
    let backend = format!("{}/v1", server.uri());

    let cfg = load_case_config(
        &dir,
        &backend,
        "reasoning_effort = \"medium\"\n",
        &coder_section("reasoning_effort = \"high\"\nvalidator_reasoning_effort = \"low\"\n"),
    );
    let coder = cfg
        .orchestration
        .specialists
        .get("coder")
        .expect("the coder section must be present");
    assert_eq!(
        coder.validator_reasoning_effort.as_deref(),
        Some("low"),
        "`validator_reasoning_effort` must parse next to `validator_model`"
    );

    drive_delegation_round(&server, &cfg, Agent::Coder).await;

    let bodies = captured_bodies(&server).await;
    let validator_bodies = bodies_from(&bodies, VALIDATOR_MODEL);
    assert_eq!(
        validator_bodies.len(),
        1,
        "`max_validator_iterations = 1` pins exactly one validator pass"
    );
    assert_role_sends(VALIDATOR_MODEL, &bodies, "low");
    assert_role_sends(CODER_MODEL, &bodies, "high");

    // Spelled out explicitly, so a regression that copies the specialist value
    // into the validator run fails here even if `"low"` were ever renamed.
    let coder_bodies = bodies_from(&bodies, CODER_MODEL);
    let coder_effort = wire_effort(&coder_bodies[0]);
    let validator_effort = wire_effort(&validator_bodies[0]);
    assert_ne!(
        validator_effort, coder_effort,
        "a validator pass must never reuse the specialist's own dial (coder {coder_effort:?}, validator {validator_effort:?})"
    );
}

/// (4) A role that sets **no** override falls back to the top-level value — even
/// while a different role's section carries its own values (the researcher decoy).
#[tokio::test]
async fn role_without_an_override_falls_back_to_the_top_level_value() {
    let dir = tempfile::tempdir().expect("tempdir for the throwaway marmel.toml");
    let server = role_routed_backend().await;
    let backend = format!("{}/v1", server.uri());

    let cfg = load_case_config(
        &dir,
        &backend,
        "reasoning_effort = \"medium\"\n",
        &format!("{}\n{RESEARCHER_DECOY}", coder_section("")),
    );
    let coder = cfg
        .orchestration
        .specialists
        .get("coder")
        .expect("the coder section must be present");
    assert!(
        coder.reasoning_effort.is_none() && coder.validator_reasoning_effort.is_none(),
        "the coder section sets no effort key at all"
    );

    drive_delegation_round(&server, &cfg, Agent::Coder).await;

    let bodies = captured_bodies(&server).await;
    assert_role_sends(CODER_MODEL, &bodies, "medium");
    assert_role_sends(VALIDATOR_MODEL, &bodies, "medium");
    // Per-role isolation: no value from another role's section may leak into this
    // round's bodies.
    for body in &bodies {
        assert!(
            wire_effort(body) != Some("ultra") && wire_effort(body) != Some("maximal"),
            "the researcher's own values leaked into a coder round: {body}"
        );
    }
}

/// (5) Nothing set anywhere ⇒ no `reasoning_effort` key in **any** captured body:
/// not the Manager turn, not the specialist turn, not the validator pass.
#[tokio::test]
async fn nothing_set_sends_no_reasoning_effort_key_anywhere() {
    let dir = tempfile::tempdir().expect("tempdir for the throwaway marmel.toml");
    let server = role_routed_backend().await;
    let backend = format!("{}/v1", server.uri());

    let cfg = load_case_config(&dir, &backend, "", &coder_section(""));
    assert!(
        cfg.reasoning_effort.is_none(),
        "an absent top-level key must stay `None`"
    );
    let coder = cfg
        .orchestration
        .specialists
        .get("coder")
        .expect("the coder section must be present");
    assert!(
        coder.reasoning_effort.is_none() && coder.validator_reasoning_effort.is_none(),
        "absent role keys must stay `None`"
    );

    drive_manager_turn(&server, &cfg).await;
    drive_delegation_round(&server, &cfg, Agent::Coder).await;

    let bodies = captured_bodies(&server).await;
    assert_eq!(
        bodies.len(),
        3,
        "one Manager turn + one coder turn + one validator pass, got {}",
        bodies.len()
    );
    assert_role_omits(MANAGER_MODEL, &bodies);
    assert_role_omits(CODER_MODEL, &bodies);
    assert_role_omits(VALIDATOR_MODEL, &bodies);
}

/// Blank / whitespace-only values count as unset and fall through the chain
/// instead of masking the level below them: a blank role value leaves the
/// specialist on the top-level value, and a blank validator value leaves the
/// validator pass on the top-level value too. The values are still stored
/// verbatim by the parser — the normalization happens at the wire chokepoints
/// (`build_turn_request`, `build_request`).
#[tokio::test]
async fn blank_reasoning_effort_counts_as_unset_and_falls_through_the_chain() {
    let dir = tempfile::tempdir().expect("tempdir for the throwaway marmel.toml");
    let server = role_routed_backend().await;
    let backend = format!("{}/v1", server.uri());

    let cfg = load_case_config(
        &dir,
        &backend,
        "reasoning_effort = \"medium\"\n",
        &coder_section("reasoning_effort = \"   \"\nvalidator_reasoning_effort = \"\"\n"),
    );
    let coder = cfg
        .orchestration
        .specialists
        .get("coder")
        .expect("the coder section must be present");
    assert_eq!(
        (
            coder.reasoning_effort.as_deref(),
            coder.validator_reasoning_effort.as_deref()
        ),
        (Some("   "), Some("")),
        "the parser stores the configured strings verbatim; blank handling belongs to the wire chokepoint"
    );

    drive_delegation_round(&server, &cfg, Agent::Coder).await;

    let bodies = captured_bodies(&server).await;
    assert_role_sends(CODER_MODEL, &bodies, "medium");
    assert_role_sends(VALIDATOR_MODEL, &bodies, "medium");
    for body in &bodies {
        assert_ne!(
            wire_effort(body),
            Some(""),
            "a blank dial must never be sent as an empty string: {body}"
        );
    }
}
