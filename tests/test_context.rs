//! Phase A integration tests (context engine & compaction).

use marmennill::manager::ContextEngineFactory;
use marmennill::manager::context::{
    ABORTED_TOOL_RESULT, CompactionOutcome, repair_tool_call_pairs,
};
use marmennill::types::{Message, ToolCall};

#[test]
fn test_integration_context_engine_lifecycle() {
    let factory = ContextEngineFactory::new(1000);
    let mut engine = factory.manager_context(
        "You are the manager.".to_string(),
        "Build the rocket.".to_string(),
    );

    // Initial messages: system prompt + goal prompt
    assert_eq!(engine.messages().len(), 2);

    // Add user and assistant messages
    engine.append(Message::User {
        content: "Please check the rocket engine.".to_string(),
    });
    engine.append(Message::Assistant {
        content: Some("I have verified the engine.".to_string()),
        reasoning_content: None,
        tool_calls: vec![],
    });

    assert_eq!(engine.messages().len(), 4);

    // Test rebirth preserves locked header
    engine.perform_rebirth("Engine verification complete.");
    let msgs = engine.messages();
    assert_eq!(msgs.len(), 4);
    assert!(msgs[0].content().unwrap().contains("You are the manager"));
    assert!(msgs[1].content().unwrap().contains("Build the rocket"));
    assert!(
        msgs[2]
            .content()
            .unwrap()
            .contains("Please check the rocket engine")
    );
    assert!(
        msgs[3]
            .content()
            .unwrap()
            .contains("Engine verification complete")
    );
}

// ---------------------------------------------------------------------------
// H1 — the compacted transcript must always be a valid OpenAI-compatible chat
// request: every assistant `tool_calls` entry is followed by exactly one `Tool`
// message per call id, and every `Tool` message has a surviving parent.
// ---------------------------------------------------------------------------

/// Assistant message carrying the given tool call ids.
fn assistant_with_tool_calls(ids: &[&str]) -> Message {
    Message::Assistant {
        content: Some("calling tools".to_string()),
        reasoning_content: None,
        tool_calls: ids
            .iter()
            .map(|id| ToolCall::new(*id, "read_file", r#"{"path":"x"}"#))
            .collect(),
    }
}

/// tool_call ids on surviving assistants that have no `Tool` result.
fn dangling_tool_call_ids(messages: &[Message]) -> Vec<String> {
    let tool_ids: std::collections::HashSet<&str> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.as_str()),
            _ => None,
        })
        .collect();
    messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .filter(|tc| !tool_ids.contains(tc.id.as_str()))
        .map(|tc| tc.id.clone())
        .collect()
}

/// `Tool` messages with no surviving assistant parent.
fn orphan_tool_ids(messages: &[Message]) -> Vec<String> {
    let assistant_ids: std::collections::HashSet<&str> = messages
        .iter()
        .filter_map(|m| match m {
            Message::Assistant { tool_calls, .. } => Some(tool_calls),
            _ => None,
        })
        .flatten()
        .map(|tc| tc.id.as_str())
        .collect();
    messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { tool_call_id, .. }
                if !assistant_ids.contains(tool_call_id.as_str()) =>
            {
                Some(tool_call_id.clone())
            }
            _ => None,
        })
        .collect()
}

/// Assert the strict OpenAI-compatible block shape over the whole transcript.
fn assert_valid_openai_pairing(messages: &[Message]) {
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let mut j = i + 1;
                for tc in tool_calls {
                    assert!(
                        j < messages.len(),
                        "assistant tool_call {} is not followed by a Tool message",
                        tc.id
                    );
                    match &messages[j] {
                        Message::Tool { tool_call_id, .. } => {
                            assert_eq!(tool_call_id, &tc.id, "mispaired tool result at index {j}");
                        }
                        other => panic!(
                            "expected Tool for tool_call {} at index {j}, got {}",
                            tc.id,
                            other.role()
                        ),
                    }
                    j += 1;
                }
                assert!(
                    !matches!(messages.get(j), Some(Message::Tool { .. })),
                    "extra Tool message after assistant block at index {i}"
                );
                i = j;
            }
            Message::Tool { tool_call_id, .. } => {
                panic!("orphan Tool message {tool_call_id} at index {i}");
            }
            _ => i += 1,
        }
    }
    assert!(dangling_tool_call_ids(messages).is_empty());
    assert!(orphan_tool_ids(messages).is_empty());
}

/// Live-shaped regression: the Manager transcript ends on an assistant turn
/// whose tool loop was aborted, so some calls never got a result. Compaction
/// (as driven from `src/ui/session.rs`) must not upload a dangling
/// `tool_calls` entry.
#[test]
fn test_integration_compaction_keeps_tool_call_pairing_valid() {
    let factory = ContextEngineFactory::new(400);
    let mut engine = factory.manager_context(
        "You are the manager.".to_string(),
        "Build the rocket. Detailed goal description with a few extra words of padding."
            .to_string(),
    );

    for i in 0..10 {
        engine.append(Message::User {
            content: format!(
                "Verbose steering instruction number {i} with padding text to consume tokens."
            ),
        });
        engine.append(Message::Assistant {
            content: Some(format!(
                "Verbose manager acknowledgement {i} with padding text here."
            )),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    // Aborted tool loop: 3 calls requested, only 1 result produced.
    engine.append(assistant_with_tool_calls(&["live_a", "live_b", "live_c"]));
    engine.append(Message::Tool {
        tool_call_id: "live_a".to_string(),
        content: "tool output".to_string(),
    });
    // Plus a stray/orphan result with no parent at all.
    engine.append(Message::Tool {
        tool_call_id: "live_orphan".to_string(),
        content: "orphan output".to_string(),
    });

    engine.compact();

    let msgs = engine.messages();
    assert!(matches!(msgs[0], Message::System { .. }));
    assert!(matches!(msgs[1], Message::User { .. }));
    assert_valid_openai_pairing(msgs);

    // The repair keeps the assistant turn and synthesizes `(aborted)` results.
    let aborted = msgs.iter().any(|m| {
        matches!(
            m,
            Message::Tool { tool_call_id, content }
                if content == ABORTED_TOOL_RESULT
                    && (tool_call_id == "live_b" || tool_call_id == "live_c")
        )
    });
    assert!(
        aborted,
        "expected `(aborted)` placeholders for the aborted tool calls"
    );
    assert!(
        !msgs.iter().any(|m| matches!(
            m,
            Message::Tool { tool_call_id, .. } if tool_call_id == "live_orphan"
        )),
        "the orphan tool result must be dropped"
    );
}

/// Helper-level contract for the public repair entry point: arbitrary broken
/// histories become valid, and valid histories are untouched.
#[test]
fn test_integration_repair_tool_call_pairs_helper() {
    let broken = vec![
        Message::System {
            content: "sys".to_string(),
        },
        Message::User {
            content: "goal".to_string(),
        },
        assistant_with_tool_calls(&["h1", "h2"]),
        Message::Tool {
            tool_call_id: "h2".to_string(),
            content: "late result".to_string(),
        },
        Message::Tool {
            tool_call_id: "ghost".to_string(),
            content: "no parent".to_string(),
        },
    ];
    let repaired = repair_tool_call_pairs(broken);
    assert_valid_openai_pairing(&repaired);
    let roles: Vec<&str> = repaired.iter().map(Message::role).collect();
    assert_eq!(roles, vec!["system", "user", "assistant", "tool", "tool"]);
    assert!(matches!(
        &repaired[3],
        Message::Tool { tool_call_id, content }
            if tool_call_id == "h1" && content == ABORTED_TOOL_RESULT
    ));
    assert!(matches!(
        &repaired[4],
        Message::Tool { tool_call_id, content }
            if tool_call_id == "h2" && content == "late result"
    ));
}

// ---------------------------------------------------------------------------
// M5 — compaction must fail loudly when the 70% target is unreachable, and the
// pinned prefix must be part of the budget arithmetic.
// ---------------------------------------------------------------------------

/// Long enough that the pinned prefix alone can exceed a small budget.
const LONG_SYSTEM_PROMPT: &str = "You are the manager. You must read files before editing them. You must verify every change with the available test commands. Never guess at file contents. Always report findings precisely with file paths and line numbers. You must delegate implementation work to specialists. You must keep the plan file on disk up to date and never output conversational filler. You must keep the plan file on disk up to date and never output conversational filler.";

/// Fill a transcript past the 90% trigger of a small budget.
fn fill(engine: &mut marmennill::manager::ContextEngine, turns: usize) {
    for i in 0..turns {
        engine.append(Message::User {
            content: format!("Verbose steering instruction number {i} with padding text."),
        });
        engine.append(Message::Assistant {
            content: Some(format!("Verbose manager acknowledgement number {i} here.")),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
}

#[test]
fn test_integration_compaction_reports_failure_for_unreachable_target() {
    let budget = 100usize;
    // Ratio constants are unchanged by the M5 fix.
    assert_eq!(
        marmennill::manager::context::compaction_target(budget),
        70,
        "target stays at 0.70 of the budget"
    );
    assert_eq!(
        marmennill::manager::context::compaction_threshold(budget),
        90,
        "trigger stays at 0.90 of the budget"
    );

    let mut engine = ContextEngineFactory::new(budget).manager_context(
        LONG_SYSTEM_PROMPT.to_string(),
        "Build the rocket.".to_string(),
    );
    fill(&mut engine, 8);
    assert!(engine.should_compact());

    let outcome = engine.compact();
    assert!(
        matches!(outcome, CompactionOutcome::TargetUnreachable { .. }),
        "an over-budget pinned prefix must fail, not silently under-deliver: {outcome:?}"
    );
    assert!(!outcome.succeeded());
    assert!(
        engine.token_count() > marmennill::manager::context::compaction_target(budget),
        "the transcript really is over budget, which is why compaction must report failure"
    );
    // The pin survives the failure (REQ-CORE-001/002).
    assert!(matches!(engine.messages()[0], Message::System { .. }));
    assert_eq!(
        engine.messages()[1].content().unwrap(),
        "Build the rocket.",
        "the pinned goal survives a failed compaction"
    );
    assert_valid_openai_pairing(engine.messages());

    // Same content with a reachable budget still lands on the 70% target.
    let big = 300usize;
    let mut engine = ContextEngineFactory::new(big).manager_context(
        LONG_SYSTEM_PROMPT.to_string(),
        "Build the rocket.".to_string(),
    );
    fill(&mut engine, 8);
    let before = engine.messages().len();
    let outcome = engine.compact();
    assert!(
        outcome.succeeded(),
        "a reachable target must still succeed: {outcome:?}"
    );
    assert!(engine.token_count() <= marmennill::manager::context::compaction_target(big));
    assert_eq!(
        outcome.messages_removed(),
        before - engine.messages().len(),
        "the reported count is the delta of the final vector"
    );
    assert!(
        outcome.messages_removed() > 0,
        "the window must actually trim"
    );
    assert_valid_openai_pairing(engine.messages());
}

#[test]
fn test_integration_rebirth_checkpoint_shape_after_injected_advisory() {
    let mut engine = ContextEngineFactory::new(2000).manager_context(
        "You are the manager.".to_string(),
        "Build the rocket.".to_string(),
    );
    engine.append(Message::User {
        content: "Check the rocket engine pressure.".to_string(),
    });
    // The runtime injects its own advisory just before the model calls rebirth.
    engine.inject_rebirth_advisory();

    engine.perform_rebirth("Engine pressure verified at 4.2 bar.");
    let msgs = engine.messages();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[0].role(), "system");
    assert_eq!(msgs[1].role(), "user");
    assert_eq!(msgs[1].content().unwrap(), "Build the rocket.");
    // M6: the last genuine instruction wins slot [2], not the injected advisory.
    assert_eq!(
        msgs[2].content().unwrap(),
        "Check the rocket engine pressure."
    );
    assert_ne!(
        msgs[2].content().unwrap(),
        marmennill::manager::context::REBIRTH_ADVISORY_MESSAGE
    );
    // M12: the checkpoint is a User turn and the goal appears exactly once.
    let last = msgs.last().unwrap();
    assert_eq!(last.role(), "user");
    assert!(
        last.content()
            .unwrap()
            .starts_with(marmennill::manager::context::REBIRTH_CHECKPOINT_PREFIX)
    );
    assert_eq!(
        msgs.iter()
            .filter(|m| m.content().unwrap_or_default() == "Build the rocket.")
            .count(),
        1,
        "the pinned goal must not be duplicated by the checkpoint"
    );
    assert_valid_openai_pairing(msgs);
}

// ── Gate t-064: the tool-schema term must be LIVE on the production paths ─────
//
// `ContextEngine::request_token_count()` and `should_compact()` price the whole
// request (transcript **plus** the advertised tool schemas). That is only true if
// the live Manager/specialist paths actually declare the list they send; an engine
// built with a bare `ContextEngine::new` and no `set_tools` call silently budgets
// messages alone. These tests pin the wiring, not just the arithmetic.

const MANAGER_SYSTEM: &str = "You are the manager.";

/// The live Manager engine must charge a schema term that is (a) non-zero and
/// (b) numerically equal to the tools list the Manager wire path assembles — and
/// therefore `request_token_count()` is **strictly above** the message-only
/// baseline for a realistic tool list.
#[test]
fn test_live_manager_context_charges_its_advertised_tool_schema() {
    use marmennill::config::Config;
    use marmennill::manager::context::{manager_wire_tools, tools_tokens};
    use marmennill::ui::session::build_manager_context;

    let cfg = Config::default();
    let advertised = manager_wire_tools(&cfg.orchestration.mcp_servers);
    let schema = tools_tokens(&advertised);
    assert!(
        schema > 0,
        "the Manager advertised list ({}) must carry schema tokens",
        advertised.len()
    );

    let engine = build_manager_context(&cfg, MANAGER_SYSTEM.to_string());
    let messages = engine.token_count();
    assert_eq!(
        engine.tool_schema_tokens(),
        schema,
        "the live Manager engine must charge exactly the advertised Manager list"
    );
    assert_eq!(engine.request_token_count(), messages + schema);
    assert!(
        engine.request_token_count() > messages,
        "regression (gate t-064): the request size must exceed the message-only baseline"
    );

    // Every built-in Manager tool is inside the charged list: the charge is never
    // a subset of what the wire sends.
    let charged: Vec<&str> = advertised
        .iter()
        .map(|t| t.function.name.as_str())
        .collect();
    for tool in marmennill::types::ToolDef::manager_tools() {
        assert!(
            charged.contains(&tool.function.name.as_str()),
            "charged schema list is missing advertised tool {}",
            tool.function.name
        );
    }

    // A wiped charge is healed by the per-decision re-declaration the session uses.
    let mut engine = build_manager_context(&cfg, MANAGER_SYSTEM.to_string());
    engine.set_tools(&[]);
    assert_eq!(engine.tool_schema_tokens(), 0);
    let restored = marmennill::ui::session::sync_manager_tool_schema(&mut engine, &cfg);
    assert_eq!(restored, schema);
}

/// The same transcript under the same budget must compact **earlier** once the
/// live engine charges its schemas: the message-only baseline stays below the
/// 90% trigger while the real request is above it.
#[test]
fn test_live_manager_compaction_triggers_earlier_than_message_only_baseline() {
    use marmennill::config::Config;
    use marmennill::manager::context::compaction_threshold;
    use marmennill::manager::context::manager_wire_tools;
    use marmennill::ui::session::build_manager_context;

    let cfg = Config::default();
    let probe = build_manager_context(&cfg, MANAGER_SYSTEM.to_string());
    let messages = probe.token_count();
    let schema = probe.tool_schema_tokens();
    assert!(schema > 0);

    // Pick a budget whose 90% trigger falls strictly between the message-only
    // baseline and the real (message + schema) request size.
    let midpoint = messages + schema / 2;
    let budget = ((midpoint as f64) / 0.9).round() as usize;
    let threshold = compaction_threshold(budget);
    assert!(
        threshold > messages && threshold < messages + schema,
        "budget probe mis-calibrated: messages={messages} schema={schema} \
         budget={budget} threshold={threshold}"
    );

    let cfg = Config {
        max_context_tokens: budget,
        ..Config::default()
    };
    let ctx = build_manager_context(&cfg, MANAGER_SYSTEM.to_string());
    assert_eq!(ctx.token_count(), messages);
    assert!(
        ctx.should_compact(),
        "live request ({} transcript + {} schema = {}) must cross the trigger at {}",
        messages,
        schema,
        ctx.request_token_count(),
        threshold
    );
    let mut baseline_ctx = ctx.clone();
    baseline_ctx.set_tools(&[]);
    assert!(
        !baseline_ctx.should_compact(),
        "regression (gate t-064): the message-only baseline ({messages}) must be the one that misses"
    );
    assert!(!ctx.should_compact_with_tools(&[]));
    assert!(ctx.should_compact_with_tools(&manager_wire_tools(&cfg.orchestration.mcp_servers)));
    assert_eq!(
        ctx.should_compact_with_tools(&manager_wire_tools(&cfg.orchestration.mcp_servers)),
        ctx.should_compact(),
        "the declared charge must equal the advertised list"
    );
}

/// The specialist side of the same bug: the role-filtered list a specialist
/// advertises is the authority, and charging it raises the measured request size
/// (and moves the compaction trigger) exactly as the live Manager path does.
#[test]
fn test_specialist_advertised_list_precharge_raises_request_size() {
    use marmennill::agents::Agent;
    use marmennill::agents::runner::execution::specialist_advertised_tools;
    use marmennill::agents::runner::fix_loop::charge_engine_tool_schema;
    use marmennill::harness::ToolCaller;
    use marmennill::manager::ContextEngineFactory;

    let caller = ToolCaller::Specialist(Agent::Coder);
    let advertised = specialist_advertised_tools(&caller, |_| true, &[]);
    assert!(
        !advertised.is_empty(),
        "a specialist advertises a role-filtered tool list"
    );

    let messages = 4000;
    let factory = ContextEngineFactory::new(messages);
    let mut engine = factory.specialist_context(
        "You are the coder.".to_string(),
        "repair the crash".to_string(),
    );
    engine.append(marmennill::types::Message::User {
        content: "the reader panics on an empty buffer".to_string(),
    });

    let baseline = engine.request_token_count();
    assert_eq!(engine.tool_schema_tokens(), 0);
    assert_eq!(baseline, engine.token_count());

    let charged = charge_engine_tool_schema(&mut engine, &advertised);
    assert!(charged > 0, "a specialist advertises real schemas");
    assert_eq!(engine.tool_schema_tokens(), charged);
    assert_eq!(engine.request_token_count(), baseline + charged);
    assert!(
        engine.request_token_count() > baseline,
        "regression (gate t-064): a specialist request carrying tools is bigger than its transcript"
    );

    // Same transcript, budget chosen so the 90% trigger sits between the
    // message-only baseline and the real request: charged it compacts, uncharged
    // (the pre-fix behaviour) it does not.
    let midpoint = baseline + charged / 2;
    let budget = ((midpoint as f64) / 0.9).round() as usize;
    let threshold = marmennill::manager::context::compaction_threshold(budget);
    assert!(threshold > baseline && threshold < baseline + charged);

    let mut live = ContextEngineFactory::new(budget).specialist_context(
        "You are the coder.".to_string(),
        "repair the crash".to_string(),
    );
    live.append(marmennill::types::Message::User {
        content: "the reader panics on an empty buffer".to_string(),
    });
    assert_eq!(live.token_count(), baseline);
    assert_eq!(charge_engine_tool_schema(&mut live, &advertised), charged);
    assert!(
        live.should_compact(),
        "with the advertised schemas the specialist request ({} + {}) must cross the trigger at {}",
        baseline,
        charged,
        threshold
    );
    let mut uncharged = live.clone();
    uncharged.set_tools(&[]);
    assert!(!uncharged.should_compact());
}
