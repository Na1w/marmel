//! Integration tests for the role normalization applied before sending, in
//! `ChatClient::try_chat_once` as it applies to a **restored** session history
//! carrying a legacy `Message::System` rebirth checkpoint, and for the in-memory
//! detector half of that behaviour.
//!
//! Seam: the transport side is end-to-end — a real `ChatClient` POSTs to a socket
//! owned by `wiremock`, and the assertions read the **request body that actually
//! arrived** (`received_requests()[0].body`), never a re-serialization of the
//! input. The transcript enters the engine through the sanctioned restore seam
//! [`ContextEngine::install_restored_messages`], the documented source of a
//! mid-history `Message::System` checkpoint; the bundle → `MessageRecord` half is
//! owned by `tests/test_session_restore_manager.rs`. These tests start there: a
//! live engine vector holding `Message::System` at index > 0 about to be sent.
//!
//! What each test pins:
//!
//! 1. the checkpoint's **wire** role is `"user"` while `[0]` stays `"system"`;
//! 2. every demoted turn's content is byte-identical, and transcript order and
//!    length are untouched;
//! 3. the send leaves the checkpoint detectable in memory —
//!    `REBIRTH_CHECKPOINT_PREFIX` and `pinned_prefix_end` still find it in the
//!    engine's own vector, which still holds the `System` variant;
//! 4. a post-`perform_rebirth` history keeps the pinned-history contract
//!    (`[0]` system, `[1]` user, 4 messages, checkpoint last) and sends the
//!    checkpoint as `"user"`.
//!
//! `types::Message` has no `PartialEq`, so every comparison goes through
//! `role()`/`content()`, `matches!`, or the captured `serde_json` value.

use marmennill::llm::client::{ChatClient, ReplyOutcome};
use marmennill::manager::context::{
    ContextEngine, REBIRTH_CHECKPOINT_PREFIX, is_injected_advisory,
};
use marmennill::session::{MessageRecord, ToolCallRecord, ToolFunctionRecord};
use marmennill::types::{ChatRequest, Message};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Pinned system prompt bytes as a restored bundle carries them.
const SYSTEM_PROMPT: &str = "SYSTEM PROMPT — pinned bytes (REQ-CORE-001)";
/// Pinned goal bytes as a restored bundle carries them.
const GOAL: &str = "goal: pinned bytes, verbatim (REQ-CORE-002)";
/// The historic `System` spelling of a rebirth checkpoint, i.e. the shape an old
/// session bundle still carries (fixture shape in `src/session/manager_capture.rs`).
const LEGACY_CHECKPOINT_1: &str = "(SYSTEM: REBIRTH CHECKPOINT. The previous turn-by-turn history has been compacted. Summarized progress: 3 turns distilled: goal pinned at [1], loader shipped.)";
/// A second legacy checkpoint from an even older build, further down the same
/// restored transcript.
const LEGACY_CHECKPOINT_2: &str = "(SYSTEM: REBIRTH CHECKPOINT. The previous turn-by-turn history has been compacted. Summarized progress: earlier compaction, older build still.)";
/// The legacy advisory marker that only old session bundles carry.
const LEGACY_ADVISORY: &str = "(SYSTEM: CONTEXT LIMIT EXCEEDED — compaction dropped 12 older turn(s). Continue from the last genuine instruction.)";

// --------------------------------------------------------------------- fixtures

/// A restored transcript in the shape a legacy session bundle produces: the
/// pinned pair, a `MessageRecord::System` rebirth checkpoint at index 2, a
/// legacy advisory, a genuine instruction, an assistant `tool_calls` turn with
/// its matching `role:"tool"` result.
fn legacy_restored_records() -> Vec<MessageRecord> {
    vec![
        MessageRecord::System {
            content: SYSTEM_PROMPT.to_string(),
        },
        MessageRecord::User {
            content: GOAL.to_string(),
        },
        MessageRecord::System {
            content: LEGACY_CHECKPOINT_1.to_string(),
        },
        MessageRecord::User {
            content: LEGACY_ADVISORY.to_string(),
        },
        MessageRecord::User {
            content: "keep going, then report".to_string(),
        },
        MessageRecord::Assistant {
            content: None,
            reasoning_content: None,
            tool_calls: vec![ToolCallRecord {
                id: "call-1".to_string(),
                kind: "function".to_string(),
                function: ToolFunctionRecord {
                    name: marmennill::tool_names::TOOL_READ_FILE.to_string(),
                    arguments: r#"{"path":"src/main.rs"}"#.to_string(),
                },
            }],
        },
        MessageRecord::Tool {
            tool_call_id: "call-1".to_string(),
            content: "first 40 lines of src/main.rs".to_string(),
        },
    ]
}

/// The same transcript extended with a second legacy `System` checkpoint at the
/// end, so several mid-history `System` turns have to be demoted at once.
fn legacy_restored_records_with_two_checkpoints() -> Vec<MessageRecord> {
    let mut records = legacy_restored_records();
    records.push(MessageRecord::System {
        content: LEGACY_CHECKPOINT_2.to_string(),
    });
    records
}

/// Install a restored transcript **wholesale** — the only sanctioned restore
/// path, which keeps every role exactly as the bundle spelled it.
fn restored_engine(records: Vec<MessageRecord>) -> ContextEngine {
    let mut engine = ContextEngine::new(8_000);
    engine.install_restored_messages(records);
    engine
}

/// The request the live turn loop would build from this engine's transcript.
fn request_for(engine: &ContextEngine) -> ChatRequest {
    ChatRequest {
        model: "test-model".to_string(),
        messages: engine.messages().to_vec(),
        tools: None,
        stream: Some(true),
        enable_thinking: None,
        reasoning_effort: None,
        temperature: None,
        top_p: None,
        presence_penalty: None,
        frequency_penalty: None,
    }
}

// ------------------------------------------------------------------ fake backend

/// Mount a real SSE backend on a real socket and return it. Every assertion in
/// this file is made against the bytes this server actually received.
async fn sse_backend() -> MockServer {
    let server = MockServer::start().await;
    let sse = format!(
        "data: {}\n\ndata: [DONE]\n\n",
        serde_json::json!({
            "id": "chatcmpl-rebirth-role",
            "choices": [{ "delta": { "content": "acknowledged" }, "finish_reason": null }]
        })
    );
    Mock::given(method("POST"))
        .and(path("/chat/completions"))
        .respond_with(ResponseTemplate::new(200).set_body_string(sse))
        .mount(&server)
        .await;
    server
}

/// Send `req` through the real client and hand back the `messages` array of the
/// body that actually reached the backend, in wire order.
async fn wire_messages(server: &MockServer, req: &ChatRequest) -> Vec<serde_json::Value> {
    let client = ChatClient::new(server.uri(), "test-model");
    let reply = client
        .chat(req)
        .await
        .expect("the backend answers the request");
    assert_eq!(
        reply.outcome,
        ReplyOutcome::Complete,
        "the request must be accepted and complete, not rejected or truncated"
    );

    let requests = server
        .received_requests()
        .await
        .expect("wiremock exposes the requests it received");
    assert_eq!(requests.len(), 1, "exactly one request body is inspected");
    let body: serde_json::Value =
        serde_json::from_slice(&requests[0].body).expect("the request body is JSON");
    body["messages"]
        .as_array()
        .expect("`messages` is a JSON array")
        .clone()
}

/// The `role` tag of one captured wire message.
fn wire_role(msg: &serde_json::Value) -> &str {
    msg["role"]
        .as_str()
        .expect("every wire message carries a role")
}

/// The `content` string of one captured wire message.
fn wire_content(msg: &serde_json::Value) -> &str {
    msg["content"].as_str().expect("content is a string")
}

// ----------------------------------------------------------------------- tests

/// (1) + (2) A restored history whose rebirth checkpoint is the legacy
/// `Message::System` spelling must go out with `role: "user"` at that index,
/// byte-identical content, and `messages[0]` still `role: "system"`.
///
/// This is the request a strict OpenAI-compatible backend otherwise rejects with
/// HTTP 400 ("System message must be at the beginning.").
#[tokio::test]
async fn test_restored_legacy_system_checkpoint_goes_out_as_user() {
    let engine = restored_engine(legacy_restored_records());
    // The fixture really is the legacy spelling — otherwise this test would assert
    // nothing about the shape it names.
    assert!(matches!(engine.messages()[2], Message::System { .. }));
    assert_eq!(engine.messages()[2].role(), "system");

    let backend = sse_backend().await;
    let wire = wire_messages(&backend, &request_for(&engine)).await;

    assert_eq!(wire.len(), 7, "no turn is added, dropped or reordered");
    assert_eq!(
        wire_role(&wire[0]),
        "system",
        "REQ-CORE-001: index 0 keeps its role"
    );
    assert_eq!(wire_role(&wire[1]), "user");
    assert_eq!(
        wire_role(&wire[2]),
        "user",
        "the legacy mid-history System checkpoint must be sent as `user`"
    );
    assert_eq!(
        wire_content(&wire[2]).as_bytes(),
        LEGACY_CHECKPOINT_1.as_bytes(),
        "only the role changes: the checkpoint bytes are preserved verbatim"
    );
    assert_eq!(wire_role(&wire[3]), "user");
    assert_eq!(
        wire_content(&wire[3]).as_bytes(),
        LEGACY_ADVISORY.as_bytes(),
        "the legacy advisory stays opaque bytes"
    );
    assert_eq!(wire_role(&wire[4]), "user");
    assert_eq!(wire_role(&wire[5]), "assistant");
    assert_eq!(wire_role(&wire[6]), "tool");

    // Exactly one `system` turn may survive on the wire, and it must be index 0.
    let systems: Vec<usize> = wire
        .iter()
        .enumerate()
        .filter(|(_, m)| wire_role(m) == "system")
        .map(|(i, _)| i)
        .collect();
    assert_eq!(systems, vec![0], "no `system` turn may exist after index 0");
}

/// (3) The same send must leave the checkpoint **detectable in memory**: the
/// engine's own vector still holds the `Message::System` variant, and the
/// detectors the compaction/rebirth path relies on — `REBIRTH_CHECKPOINT_PREFIX`
/// and `pinned_prefix_end` — still find it. The normalization runs on the cloned
/// outgoing payload only.
#[tokio::test]
async fn test_restored_legacy_system_checkpoint_stays_detectable_in_memory() {
    let engine = restored_engine(legacy_restored_records());
    let end_before = engine.pinned_prefix_end();
    assert_eq!(
        end_before, 3,
        "the checkpoint at [2] extends the pinned window"
    );

    let backend = sse_backend().await;
    let wire = wire_messages(&backend, &request_for(&engine)).await;
    assert_eq!(wire_role(&wire[2]), "user", "the wire role is demoted ...");

    // ... and the in-memory history is untouched by that demotion.
    assert_eq!(engine.messages().len(), 7, "the send never mutates history");
    assert!(
        matches!(engine.messages()[2], Message::System { .. }),
        "R-PIN-3: the in-memory checkpoint keeps its System variant, got {:?}",
        engine.messages()[2]
    );
    let content = engine.messages()[2]
        .content()
        .expect("a System message always carries content");
    assert!(
        content.starts_with(REBIRTH_CHECKPOINT_PREFIX),
        "the REBIRTH_CHECKPOINT_PREFIX detector still matches"
    );
    assert_eq!(content.as_bytes(), LEGACY_CHECKPOINT_1.as_bytes());
    assert!(
        is_injected_advisory(content),
        "still recognizable as injected"
    );
    assert_eq!(
        engine.pinned_prefix_end(),
        end_before,
        "the pinned window still ends after the checkpoint after the request"
    );
    assert_eq!(engine.messages()[0].role(), "system");
    assert_eq!(engine.messages()[1].role(), "user");
    assert_eq!(engine.messages()[1].content(), Some(GOAL));
}

/// (2) Several legacy `System` turns deep in a restored transcript are all
/// demoted at once, with their own bytes, while the leading prompt is untouched
/// and the pinned window (last checkpoint wins) is unaffected by the send.
#[tokio::test]
async fn test_every_mid_history_system_turn_is_demoted_and_index_zero_is_untouched() {
    let engine = restored_engine(legacy_restored_records_with_two_checkpoints());
    assert_eq!(
        engine.pinned_prefix_end(),
        8,
        "the last checkpoint at [7] defines the pinned window"
    );

    let backend = sse_backend().await;
    let wire = wire_messages(&backend, &request_for(&engine)).await;

    assert_eq!(wire.len(), 8);
    assert_eq!(wire_role(&wire[0]), "system");
    assert_eq!(wire_role(&wire[2]), "user");
    assert_eq!(
        wire_content(&wire[2]).as_bytes(),
        LEGACY_CHECKPOINT_1.as_bytes()
    );
    assert_eq!(wire_role(&wire[7]), "user");
    assert_eq!(
        wire_content(&wire[7]).as_bytes(),
        LEGACY_CHECKPOINT_2.as_bytes()
    );
    assert_eq!(wire_role(&wire[5]), "assistant");
    assert_eq!(wire_role(&wire[6]), "tool");

    // Both checkpoints are still System, still content-matched, in memory.
    for index in [2usize, 7] {
        assert!(
            matches!(engine.messages()[index], Message::System { .. }),
            "index {index} must keep its System variant in memory"
        );
        assert!(
            engine.messages()[index]
                .content()
                .unwrap_or_default()
                .starts_with(REBIRTH_CHECKPOINT_PREFIX),
            "index {index} must still be detected as a checkpoint"
        );
    }
    assert_eq!(engine.pinned_prefix_end(), 8);
}

/// (4) The live chain on a rehydrated session: the restored legacy history is
/// sent (`[2]` out as `user`), the in-memory vector then feeds a real
/// `perform_rebirth`, and the collapsed history still honours the pinned-history
/// contract (`test_context.rs::test_integration_rebirth_checkpoint_shape_after_injected_advisory`:
/// `[0]` system, `[1]` user, 4 messages, checkpoint last) and goes out with
/// `messages[0]` as `system` and the checkpoint as `user`.
///
/// The send before `perform_rebirth` is asserted here as well: it proves the
/// normalization never reaches back into the vector `perform_rebirth` re-reads
/// positionally — the collapse still finds the pinned prompt at `[0]`, the pinned
/// goal byte-identical at `[1]`, and the last genuine instruction — not
/// boilerplate — at `[2]`.
#[tokio::test]
async fn test_reborn_history_keeps_pin_contract_and_sends_checkpoint_as_user() {
    // Rehydrated session: the legacy `System` checkpoint is live in memory.
    let mut engine = restored_engine(legacy_restored_records());

    let first_backend = sse_backend().await;
    let first_wire = wire_messages(&first_backend, &request_for(&engine)).await;
    assert_eq!(
        wire_role(&first_wire[2]),
        "user",
        "the turn on the restored history must not send a mid-history `system` turn"
    );

    // The model now calls `rebirth` on that same, untouched vector.
    engine.perform_rebirth("Engine pressure verified at 4.2 bar.");
    let msgs = engine.messages();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0].role(), "system");
    assert_eq!(msgs[0].content(), Some(SYSTEM_PROMPT));
    assert_eq!(msgs[1].role(), "user");
    assert_eq!(msgs[1].content(), Some(GOAL));
    assert_eq!(msgs[2].content(), Some("keep going, then report"));
    let checkpoint_index = msgs.len() - 1;
    assert!(
        msgs[checkpoint_index]
            .content()
            .unwrap_or_default()
            .starts_with(REBIRTH_CHECKPOINT_PREFIX)
    );
    assert_eq!(msgs[checkpoint_index].role(), "user");

    let second_backend = sse_backend().await;
    let wire = wire_messages(&second_backend, &request_for(&engine)).await;

    assert_eq!(wire.len(), 4, "the collapsed history is sent as-is");
    assert_eq!(
        wire_role(&wire[0]),
        "system",
        "the leading system prompt must never be demoted"
    );
    for (index, msg) in wire.iter().enumerate().skip(1) {
        assert_eq!(wire_role(msg), "user", "index {index}");
    }
    assert!(
        wire_content(&wire[3]).starts_with(REBIRTH_CHECKPOINT_PREFIX),
        "the checkpoint is the last wire turn and goes out as `user`"
    );

    // The detector view is unchanged by the request.
    assert_eq!(engine.pinned_prefix_end(), 4);
    assert_eq!(engine.messages().len(), 4);
    assert_eq!(engine.messages()[0].role(), "system");
    assert!(matches!(engine.messages()[3], Message::User { .. }));
}
