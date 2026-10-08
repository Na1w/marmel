use super::*;
use crate::types::ToolCall;

/// An assistant reply long enough (≈60 tokens) to be the message that tips a
/// small transcript's 70% target, so the retention window breaks exactly there.
const VERBOSE_OVER_BUDGET_REPLY: &str = "Let me first map every call site of the parser, then re-read the tokenizer entry points, then check the regression suite output line by line before touching anything at all, because a blind edit here would silently break the manager budget model.";

/// Build a tool-response message tied to a tool call id.
fn tool_response(id: &str) -> Message {
    Message::Tool {
        tool_call_id: id.to_string(),
        content: "tool output".to_string(),
    }
}

/// Build a transcript of the given length (after the pinned system+goal).
fn fill_transcript(max_tokens: usize, extra_turns: usize) -> ContextEngine {
    let mut engine = ContextEngine::new(max_tokens);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());
    for i in 0..extra_turns {
        engine.append(Message::User {
            content: format!("Turn {i}: please make the following change to the file."),
        });
        engine.append(Message::Assistant {
            content: Some(format!("Understood, working on turn {i} now.")),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
    engine
}

#[test]
fn test_context_locking() {
    let mut engine = fill_transcript(200, 5);
    let original_system = match &engine.messages()[0] {
        Message::System { content } => content.clone(),
        _ => panic!("messages[0] should be System"),
    };
    let original_goal = match &engine.messages()[1] {
        Message::User { content } => content.clone(),
        _ => panic!("messages[1] should be User goal"),
    };

    // Append a few messages; pins must survive.
    engine.append(Message::User {
        content: "one more turn".to_string(),
    });
    assert_eq!(
        match &engine.messages()[0] {
            Message::System { content } => content,
            _ => "",
        },
        original_system
    );
    assert_eq!(
        match &engine.messages()[1] {
            Message::User { content } => content,
            _ => "",
        },
        original_goal
    );

    // Rebirth must also preserve messages[0] and messages[1].
    engine.perform_rebirth("compacted after locking test");
    assert_eq!(engine.messages().len(), 4);
    assert_eq!(
        match &engine.messages()[0] {
            Message::System { content } => content,
            _ => "",
        },
        original_system
    );
    assert_eq!(
        match &engine.messages()[1] {
            Message::User { content } => content,
            _ => "",
        },
        original_goal
    );
}

/// Every `tool_call_id` of the `Tool` messages in `messages`, in order.
fn tool_ids(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .filter_map(|m| match m {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect()
}

/// Witness of the **positional tail-keep alone**: what `compact()` would retain
/// if the pairing repair (`prune_orphan_tool_messages` + placeholder synthesis)
/// never ran. Replicates the retention arithmetic of `ContextEngine::compact`
/// (pinned prefix, then the newest messages as long as they fit the target).
///
/// Used to prove that an id missing from the compacted transcript really was
/// dropped by the repair pass: if the tail-keep alone already dropped it, the
/// assertion "the orphan was pruned" proves nothing (recon item M13).
fn window_only_transcript(messages: &[Message], target: usize) -> Vec<Message> {
    let end = pinned_prefix_end(messages).min(messages.len());
    let mut kept = messages[..end].to_vec();
    let mut kept_tail: Vec<Message> = Vec::new();
    let mut total = count_tokens(&messages[..end]);
    for m in messages[end..].iter().rev() {
        let cost = count_tokens(std::slice::from_ref(m));
        if total + cost > target && !kept_tail.is_empty() {
            break;
        }
        total += cost;
        kept_tail.push(m.clone());
    }
    kept_tail.reverse();
    kept.extend(kept_tail);
    kept
}

/// REQ-CORE-003 / M13: the half of the pairing invariant that
/// `prune_orphan_tool_messages` owns must be exercised at the `compact()` level
/// by the **pair-split** shape — the assistant that owns a `Tool` result leaves
/// the retention window while that result stays inside it.
///
/// The fixture is built so the assertion is discriminating:
/// * `call_split_parent`'s assistant is the message that tips the target, so the
///   tail-keep drops the parent and keeps the result — the result is an orphan
///   *only* because its owner left the window;
/// * `call_never_parented` is a result that never had an assistant at all;
/// * the **newest** message is `call_newest`'s legitimate result, whose parent is
///   also kept: it must survive verbatim, so the test cannot pass for an
///   implementation that "fixes" orphans by dropping the newest tool result.
///
/// The `window_only_transcript` witness pins the fixture itself: the tail-keep
/// alone retains all three ids, therefore their absence after `compact()` can
/// only be attributed to the repair pass.
#[test]
fn test_compaction_prunes_orphans_left_by_the_window_and_keeps_the_newest_result() {
    let budget = 140usize;
    let target = compaction_target(budget);
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());

    // Old turns the window is expected to drop wholesale (over the 90% trigger).
    push_over_trigger(&mut engine, 6);

    // The oversized assistant turn owns `call_split_parent`. It is the message
    // that tips the 70% target, so the tail-keep breaks exactly here: the parent
    // is dropped, its result (below it) is kept.
    engine.append(Message::Assistant {
        content: Some(VERBOSE_OVER_BUDGET_REPLY.to_string()),
        reasoning_content: None,
        tool_calls: vec![ToolCall::new(
            "call_split_parent",
            crate::tool_names::TOOL_READ_FILE,
            r#"{"path":"src/manager/context.rs"}"#,
        )],
    });
    engine.append(tool_response("call_split_parent"));
    // A second orphan inside the window: this one was never parented at all.
    engine.append(tool_response("call_never_parented"));
    // The newest pair, fully inside the window: both sides must survive.
    engine.append(assistant_with_tool_calls(&["call_newest"]));
    engine.append(tool_response("call_newest"));

    let original = engine.messages().to_vec();
    assert!(
        engine.should_compact(),
        "fixture: transcript must exceed the 90% trigger"
    );

    // Fixture guard: the tail-keep alone keeps all three tool results, i.e. this
    // really is the pair-split case and not a positional drop.
    let window = window_only_transcript(&original, target);
    let window_ids = tool_ids(&window);
    for id in ["call_split_parent", "call_never_parented", "call_newest"] {
        assert!(
            window_ids.iter().any(|kept| kept == id),
            "fixture: the retention window alone must retain {id} \
             (otherwise the repair pass is never exercised), got {window_ids:?}"
        );
    }
    assert!(
        !window.iter().any(|m| matches!(
            m,
            Message::Assistant { tool_calls, .. }
                if tool_calls.iter().any(|tc| tc.id == "call_split_parent")
        )),
        "fixture: the oversized assistant owning `call_split_parent` must fall out of the window"
    );

    let outcome = engine.compact();
    let final_ids = tool_ids(engine.messages());

    // Both orphans are gone even though the window kept them.
    assert!(
        !final_ids.iter().any(|id| id == "call_split_parent"),
        "the result whose assistant left the window must be pruned, got {final_ids:?}"
    );
    assert!(
        !final_ids.iter().any(|id| id == "call_never_parented"),
        "a result that never had an assistant must be pruned, got {final_ids:?}"
    );
    assert!(
        !engine.messages().iter().any(|m| matches!(
            m,
            Message::Assistant { tool_calls, .. }
                if tool_calls.iter().any(|tc| tc.id == "call_split_parent")
        )),
        "the oversized assistant must have been dropped by the window"
    );

    // The newest legitimate result survives — exactly one result, verbatim, next
    // to its surviving parent.
    assert_eq!(
        final_ids,
        vec!["call_newest".to_string()],
        "the newest tool result must be preserved and no orphan may remain"
    );
    let idx = engine
        .messages()
        .iter()
        .position(|m| matches!(m, Message::Assistant { tool_calls, .. } if tool_calls.iter().any(|tc| tc.id == "call_newest")))
        .expect("the assistant owning the newest result must survive");
    assert!(
        matches!(
            &engine.messages()[idx + 1],
            Message::Tool { tool_call_id, content }
                if tool_call_id == "call_newest" && content == "tool output"
        ),
        "the newest result must keep its content and its position under its parent"
    );

    // Pins and budget are unaffected by the repair.
    assert!(matches!(engine.messages()[0], Message::System { .. }));
    assert!(matches!(engine.messages()[1], Message::User { .. }));
    assert!(outcome.succeeded(), "the pinned prefix fits: {outcome:?}");
    assert!(
        engine.token_count() <= target,
        "compact must land on the 70% target (got {})",
        engine.token_count()
    );
    assert_eq!(outcome.final_tokens(), engine.token_count());
    assert_eq!(
        outcome.messages_removed(),
        original.len() - engine.messages().len()
    );
    assert_valid_openai_pairing(engine.messages());
}

#[test]
fn test_context_rebirth_reconstruction() {
    let stats = Arc::new(HarnessStats::new());
    let mut engine = fill_transcript(500, 6);
    engine.set_stats(stats.clone());
    engine.append(Message::User {
        content: "Final instruction distinct from the goal.".to_string(),
    });

    engine.perform_rebirth("rewrote the module under test");

    let msgs = engine.messages();
    assert_eq!(
        msgs.len(),
        4,
        "rebirth collapses to 4 messages when a distinct last instruction exists"
    );

    // [0] system, [1] goal, [2] last user instruction, [3] checkpoint (M12: the
    // checkpoint is a User turn, never a second system turn).
    assert!(matches!(msgs[0], Message::System { .. }));
    assert!(matches!(msgs[1], Message::User { .. }));
    match &msgs[2] {
        Message::User { content } => {
            assert_eq!(content, "Final instruction distinct from the goal.")
        }
        _ => panic!("messages[2] should be the last user instruction"),
    }
    match &msgs[3] {
        Message::User { content } => {
            assert!(
                content.starts_with(REBIRTH_CHECKPOINT_PREFIX),
                "messages[3] must be the REBIRTH CHECKPOINT injection"
            );
            assert!(content.contains("rewrote the module under test"));
        }
        _ => panic!("messages[3] should be the checkpoint User message"),
    }
    // t-031a pairing invariant: a collapsed transcript is a valid request.
    assert_valid_openai_pairing(msgs);

    // The session_rebirths counter must be incremented.
    assert_eq!(
        stats
            .session_rebirths
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn test_context_factory_manager_prefix_locked() {
    let f = ContextEngineFactory::new(2048);
    let mut ctx = f.manager_context(
        "You are the Manager.".to_string(),
        "Ship the feature.".to_string(),
    );
    // Manager prefix: [0]=system, [1]=goal (REQ-CORE-001/002).
    assert!(matches!(ctx.messages()[0], Message::System { .. }));
    assert!(matches!(ctx.messages()[1], Message::User { .. }));

    // Appending turns must never move the pinned prefix.
    ctx.append(Message::User {
        content: "steer".to_string(),
    });
    assert_eq!(
        match &ctx.messages()[0] {
            Message::System { content } => content,
            _ => "",
        },
        "You are the Manager."
    );
    assert_eq!(
        match &ctx.messages()[1] {
            Message::User { content } => content,
            _ => "",
        },
        "Ship the feature."
    );
}

#[test]
fn test_context_factory_specialist_isolated_prefix() {
    let f = ContextEngineFactory::new(2048);
    let mut spec = f.specialist_context(
        "You are the Coder specialist.".to_string(),
        "Implement the parser.".to_string(),
    );
    // Exactly two seeded messages: role at [0], brief at [1] (REQ-ORCH-003).
    assert_eq!(spec.messages().len(), 2);
    match &spec.messages()[0] {
        Message::System { content } => assert_eq!(content, "You are the Coder specialist."),
        _ => panic!("specialist messages[0] must be the role system prompt"),
    }
    match &spec.messages()[1] {
        Message::User { content } => assert_eq!(content, "Implement the parser."),
        _ => panic!("specialist messages[1] must be the task brief goal"),
    }

    // Isolation: append local work; [0]/[1] stay pinned.
    spec.append(Message::Assistant {
        content: Some("on it".to_string()),
        reasoning_content: None,
        tool_calls: vec![],
    });
    assert!(matches!(spec.messages()[0], Message::System { .. }));
    assert!(matches!(spec.messages()[1], Message::User { .. }));
}

#[test]
fn test_context_factory_specialists_isolated_from_each_other() {
    let f = ContextEngineFactory::new(2048);
    // Two distinct specialists get fully independent, prefixed engines.
    let mut coder = f.specialist_context("Coder role".to_string(), "build".to_string());
    let researcher = f.specialist_context("Researcher role".to_string(), "research".to_string());
    coder.append(Message::User {
        content: "coder-only turn".to_string(),
    });
    // The researcher engine must NOT see the coder's appended history.
    assert_eq!(
        researcher.messages().len(),
        2,
        "each specialist is freshly isolated"
    );
    assert!(matches!(researcher.messages()[0], Message::System { .. }));
    assert!(matches!(researcher.messages()[1], Message::User { .. }));
    let researcher_goal = match &researcher.messages()[1] {
        Message::User { content } => content,
        _ => "",
    };
    assert_eq!(researcher_goal, "research");
}

#[test]
fn test_rebirth_advisory_at_80_percent_and_compaction_at_90_percent() {
    let budget = 300;
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor parser.".to_string());

    let advisory_thresh = rebirth_advisory_threshold(budget); // 240
    let compact_thresh = compaction_threshold(budget); // 270

    assert!(!engine.should_advise_rebirth());
    assert!(!engine.should_compact());

    // Fill until the transcript passes the 80% advisory threshold.
    while engine.token_count() <= advisory_thresh {
        engine.append(Message::User {
            content: "Step in the plan with some text to consume tokens.".to_string(),
        });
        engine.append(Message::Assistant {
            content: Some("Working on this step now.".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    // The loop above only guarantees the LOWER bound. The band assertion below is
    // what makes the advisory half of this test non-vacuous: without it a filler
    // turn that overshoots 90% silently skips the whole advisory section.
    assert!(
        engine.token_count() > advisory_thresh && engine.token_count() <= compact_thresh,
        "fixture: transcript must land inside the 80–90% band ({advisory_thresh}..{compact_thresh}], got {}",
        engine.token_count()
    );

    assert!(
        engine.should_advise_rebirth(),
        "should advise rebirth at > 80%"
    );
    assert!(!engine.should_compact(), "should not compact yet at <= 90%");

    engine.inject_rebirth_advisory();
    assert!(engine.rebirth_advisory_emitted());
    assert!(
        !engine.should_advise_rebirth(),
        "should not advise repeatedly"
    );
    assert_eq!(
        engine.messages().last().unwrap().content().unwrap(),
        REBIRTH_ADVISORY_MESSAGE,
        "the injected advisory must be the newest message"
    );

    // Now fill further until token count exceeds 90%
    while engine.token_count() <= compact_thresh {
        engine.append(Message::User {
            content: "Additional instruction with more words to push tokens over 90% threshold."
                .to_string(),
        });
        engine.append(Message::Assistant {
            content: Some("Understood, proceeding further.".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    assert!(
        engine.should_compact(),
        "should trigger compaction above 90%"
    );
    engine.compact();
    assert!(!engine.should_compact(), "should be compacted back down");
    assert!(
        engine.token_count() <= compaction_target(budget),
        "compact targets 70% budget"
    );
    assert!(
        !engine.rebirth_advisory_emitted(),
        "advisory emitted flag should reset after compaction back below 80%"
    );
}

#[test]
fn test_rebirth_advisory_reset_on_perform_rebirth() {
    let budget = 300;
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("System prompt.".to_string());
    engine.set_goal("User goal.".to_string());

    let advisory_thresh = rebirth_advisory_threshold(budget);
    while engine.token_count() <= advisory_thresh {
        engine.append(Message::User {
            content: "More work to consume tokens.".to_string(),
        });
        engine.append(Message::Assistant {
            content: Some("Response to work.".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    assert!(
        engine.should_advise_rebirth(),
        "the filled transcript must be above the advisory threshold"
    );
    engine.inject_rebirth_advisory();
    assert!(engine.rebirth_advisory_emitted());

    // Agent executes rebirth
    engine.perform_rebirth("Finished preliminary tasks, ready for next phase.");
    assert_eq!(engine.messages().len(), 4);
    assert!(
        !engine.rebirth_advisory_emitted(),
        "rebirth resets advisory emitted flag"
    );
    assert!(
        !engine.should_advise_rebirth(),
        "collapsed context is well below 80%"
    );
}

/// The consecutive-rebirth guard must actually suppress the advisory: the same
/// over-threshold transcript advises once the counter is reset and stays silent
/// while it is non-zero. An assertion that only checks a *collapsed* transcript
/// (which is below the threshold anyway) would pass with the guard deleted, so
/// the transcript is refilled above the advisory threshold in both directions.
#[test]
fn test_consecutive_rebirth_suppresses_the_advisory_until_reset() {
    let budget = 300;
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("System prompt.".to_string());
    engine.set_goal("User goal.".to_string());

    engine.perform_rebirth("First checkpoint.");
    assert_eq!(
        engine.consecutive_rebirths(),
        1,
        "a rebirth must bump the consecutive counter"
    );

    // Refill the collapsed transcript above the advisory threshold, so the only
    // thing that can suppress the advisory is the consecutive-rebirth guard.
    while engine.token_count() <= rebirth_advisory_threshold(budget) {
        engine.append(Message::User {
            content: "More work to consume tokens in the new context window.".to_string(),
        });
        engine.append(Message::Assistant {
            content: Some("Working on the next step of the plan.".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    assert!(
        !engine.should_advise_rebirth(),
        "consecutive_rebirths > 0 must suppress the advisory even above the threshold"
    );

    // The identical transcript must advise once the counter is reset — proof the
    // suppression above came from the counter and not from the budget.
    engine.reset_consecutive_rebirths();
    assert_eq!(engine.consecutive_rebirths(), 0);
    assert!(
        engine.should_advise_rebirth(),
        "the same transcript must advise again once the counter is reset"
    );
}

#[test]
fn test_compaction_preserves_rebirth_checkpoint() {
    let budget = 300;
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("System prompt for coding agent.".to_string());
    engine.set_goal("Initial user goal for refactoring.".to_string());

    // Rebirth collapses to 3 messages here: there is no instruction distinct
    // from the goal, so the `[2]` slot is omitted instead of duplicating the
    // pinned goal (bug M12), and the checkpoint rides in at `[2]` as a User turn.
    engine
        .perform_rebirth("Vital checkpoint: inspected parser.rs at line 250, next step is ast.rs");
    assert_eq!(engine.messages().len(), 3);
    assert!(matches!(
        &engine.messages()[2],
        Message::User { content } if content.contains("Vital checkpoint")
    ));

    // Append many post-rebirth turns until token count triggers compaction (> 90% of 300 = 270 tokens).
    for i in 0..15 {
        engine.append(Message::User {
            content: format!(
                "Post-rebirth user command number {i} with verbose padding text here."
            ),
        });
        engine.append(Message::Assistant {
            content: Some(format!(
                "Assistant response to turn {i} with detailed explanation and data."
            )),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }

    assert!(
        engine.should_compact(),
        "transcript should exceed 90% budget"
    );

    // Perform automatic compaction.
    let outcome = engine.compact();
    assert!(
        outcome.succeeded(),
        "the pinned prefix of this fixture is far below the target, so compaction must succeed"
    );

    // Verify budget target:
    let target = compaction_target(budget);
    assert!(
        engine.token_count() <= target,
        "compact brings transcript to <= 70%"
    );
    assert_eq!(outcome.final_tokens(), engine.token_count());

    // Verify pins: [0] system prompt, [1] user goal.
    assert!(matches!(engine.messages()[0], Message::System { .. }));
    assert!(matches!(engine.messages()[1], Message::User { .. }));

    // CRITICAL: The rebirth checkpoint MUST survive compaction!
    let is_checkpoint = |m: &Message| match m {
        Message::User { content } | Message::System { content } => {
            content.starts_with(REBIRTH_CHECKPOINT_PREFIX) && content.contains("Vital checkpoint")
        }
        _ => false,
    };
    assert!(
        engine.messages().iter().any(is_checkpoint),
        "Rebirth checkpoint MUST survive context compaction!"
    );
    // ... and it must survive *as part of the pinned window*, which is charged
    // against the budget arithmetic since the M5 fix.
    let pinned = pinned_prefix_end(engine.messages());
    assert!(
        engine.messages()[..pinned].iter().any(is_checkpoint),
        "the rebirth checkpoint must stay inside the pinned prefix"
    );
}

/// `count_text_tokens` is the single text-level entry point for the cl100k_base
/// tokenizer: it must return a non-zero count for known ASCII text, `0` for the
/// empty string, and identical counts for repeated calls.
#[test]
fn test_count_text_tokens_basic() {
    const SAMPLE: &str = "Hello, world. This is a known ASCII string.";

    let count = count_text_tokens(SAMPLE);
    assert!(
        count > 0,
        "known ASCII text must tokenize to a non-zero count"
    );
    assert_eq!(
        count,
        count_text_tokens(SAMPLE),
        "token counting must be deterministic across calls"
    );
    assert_eq!(
        count_text_tokens(""),
        0,
        "empty text must count zero tokens"
    );
    assert!(
        count_text_tokens(&format!("{SAMPLE} plus some extra words")) > count,
        "appending text must never lower the token count"
    );
}

/// `count_tokens` must agree with `count_text_tokens` (3-token framing overhead
/// per message), proving the message-level helpers reuse the text-level helper
/// without changing numeric results. The expansions below are written out from
/// `count_text_tokens` alone, so an implementation that drops the reasoning
/// field, the tool-call name, or the serialized arguments from the budget fails
/// here instead of silently under-counting the request.
#[test]
fn test_count_tokens_reuses_count_text_tokens() {
    const BODY: &str = "Refactor the parser module and add tests.";

    assert_eq!(
        count_tokens(&[Message::User {
            content: BODY.to_string(),
        }]),
        count_text_tokens(BODY) + 3,
        "a plain message is 3 framing tokens plus its text tokens"
    );

    // A `Tool` result is charged for its content, not for the call id only.
    const RESULT: &str = "here is the requested file content, several words long";
    assert_eq!(
        count_tokens(&[Message::Tool {
            tool_call_id: "c1".to_string(),
            content: RESULT.to_string(),
        }]),
        count_text_tokens(RESULT) + 3,
        "a tool result must be charged for its content"
    );

    // An assistant turn charges content + reasoning + (1 framing token + name +
    // arguments) per tool call.
    const CONTENT: &str = "Reading the module now.";
    const REASONING: &str = "I should inspect the parser before editing it.";
    const ARGS: &str = r#"{"path":"src/manager/context.rs"}"#;
    let calls = vec![ToolCall::new("c1", crate::tool_names::TOOL_READ_FILE, ARGS)];
    let expanded = 3
        + count_text_tokens(CONTENT)
        + count_text_tokens(REASONING)
        + 1
        + count_text_tokens(crate::tool_names::TOOL_READ_FILE)
        + count_text_tokens(ARGS);
    assert_eq!(
        count_tokens(&[Message::Assistant {
            content: Some(CONTENT.to_string()),
            reasoning_content: Some(REASONING.to_string()),
            tool_calls: calls.clone(),
        }]),
        expanded,
        "the message-level count must charge content, reasoning and every tool call"
    );
    assert_eq!(
        count_assistant_tokens(Some(CONTENT), Some(REASONING), &calls),
        expanded - 3,
        "the turn-level helper must charge the same terms without the framing"
    );
    assert!(
        count_assistant_tokens(None, None, &calls) > 0,
        "a tool call with no prose must still be charged, otherwise the budget \
         model under-counts tool-heavy turns"
    );
}

// ---------------------------------------------------------------------------
// H1 — assistant `tool_calls` <-> `Tool` pairing invariant
//
// Invariant enforced by `compact()` (bug H1, `docs/recon_bugs_manager.md`):
//
//   For every `Message::Assistant { tool_calls, .. }` in the compacted
//   transcript, each `tool_calls[].id` is immediately followed by exactly one
//   `Message::Tool { tool_call_id }` carrying that id (in `tool_calls` order),
//   and every `Message::Tool` has a surviving assistant parent.
//
// Missing results are synthesized as `Message::Tool { content: ABORTED_TOOL_RESULT }`
// (option (a) — see the comment on `repair_tool_call_pairs`).
// ---------------------------------------------------------------------------

/// Build an assistant message carrying N tool calls with the given ids.
fn assistant_with_tool_calls(ids: &[&str]) -> Message {
    Message::Assistant {
        content: Some("let me call some tools".to_string()),
        reasoning_content: None,
        tool_calls: ids
            .iter()
            .map(|id| ToolCall::new(*id, crate::tool_names::TOOL_READ_FILE, r#"{"path":"x"}"#))
            .collect(),
    }
}

/// Append verbose filler turns to push the transcript over the compaction trigger.
fn push_over_trigger(engine: &mut ContextEngine, turns: usize) {
    for i in 0..turns {
        engine.append(Message::User {
            content: format!(
                "This is a fairly verbose user instruction number {i} with padding text to consume many tokens."
            ),
        });
        engine.append(Message::Assistant {
            content: Some(format!(
                "Assistant acknowledging instruction {i} with a lengthy verbose reply full of detail."
            )),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
}

/// Collect every `tool_calls` id on a surviving assistant message that has no
/// matching `Message::Tool` anywhere in the sequence (a dangling tool call).
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

/// Collect every `Message::Tool` whose id has no surviving assistant parent.
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

/// Assert the full OpenAI-compatible chat-request shape of a transcript:
///
/// - every assistant carrying `tool_calls` is followed *immediately* by exactly
///   one `Tool` message per call id (same ids, same order, nothing in between);
/// - no `Tool` message appears outside such a block (i.e. no orphan `Tool`);
/// - no duplicate `tool_call_id` in the transcript.
fn assert_valid_openai_pairing(messages: &[Message]) {
    // 1. Duplicate ids are rejected (assistant ids and result ids tracked apart).
    let mut seen_call_ids = std::collections::HashSet::new();
    let mut seen_result_ids = std::collections::HashSet::new();
    for m in messages {
        match m {
            Message::Assistant { tool_calls, .. } => {
                for tc in tool_calls {
                    assert!(
                        seen_call_ids.insert(tc.id.clone()),
                        "duplicate tool_call id {}",
                        tc.id
                    );
                }
            }
            Message::Tool { tool_call_id, .. } => {
                assert!(
                    seen_result_ids.insert(tool_call_id.clone()),
                    "duplicate tool result id {tool_call_id}"
                );
            }
            _ => {}
        }
    }

    // 2. Walk the sequence: each assistant block must be followed by exactly
    //    its tool results, and no stray `Tool` may follow the block.
    let mut i = 0;
    while i < messages.len() {
        match &messages[i] {
            Message::Assistant { tool_calls, .. } if !tool_calls.is_empty() => {
                let mut j = i + 1;
                for tc in tool_calls {
                    assert!(
                        j < messages.len(),
                        "assistant tool_call {} has no following Tool message (assistant at index {i})",
                        tc.id
                    );
                    match &messages[j] {
                        Message::Tool { tool_call_id, .. } => {
                            assert_eq!(
                                tool_call_id, &tc.id,
                                "Tool message at index {j} does not match tool_call {}",
                                tc.id
                            );
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
                panic!("orphan Tool message {tool_call_id} at index {i} (no assistant parent)");
            }
            _ => i += 1,
        }
    }

    // 3. Belt-and-braces: neither half of the invariant may report a violation.
    assert!(
        dangling_tool_call_ids(messages).is_empty(),
        "dangling tool_calls: {:?}",
        dangling_tool_call_ids(messages)
    );
    assert!(
        orphan_tool_ids(messages).is_empty(),
        "orphan Tool messages: {:?}",
        orphan_tool_ids(messages)
    );
}

/// Regression (H1): an assistant with N `tool_calls` survives compaction while
/// some of its `Tool` results never arrived (the live trigger is
/// `src/ui/session.rs` breaking the tool loop on abort). After `compact()` the
/// missing results must be synthesized as `"(aborted)"` placeholders so the
/// sequence stays valid for an OpenAI-compatible request.
#[test]
fn test_compaction_synthesizes_aborted_result_for_dangling_tool_calls() {
    let mut engine = ContextEngine::new(400);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());

    push_over_trigger(&mut engine, 14);

    // The newest turn: an assistant asking for two tools, but only ONE result
    // ever arrived (aborted tool loop) -> `call_b` is dangling.
    engine.append(assistant_with_tool_calls(&["call_a", "call_b"]));
    engine.append(tool_response("call_a"));

    assert!(engine.should_compact(), "transcript must exceed 90% budget");
    engine.compact();

    let msgs = engine.messages();

    // Pins are untouched (REQ-CORE-001/002).
    assert!(matches!(msgs[0], Message::System { .. }));
    assert!(matches!(msgs[1], Message::User { .. }));

    // The dangling call is repaired, not silently kept.
    assert!(
        dangling_tool_call_ids(msgs).is_empty(),
        "no dangling tool_calls may survive compaction, got {:?}",
        dangling_tool_call_ids(msgs)
    );
    assert!(
        orphan_tool_ids(msgs).is_empty(),
        "no orphan Tool messages may survive compaction, got {:?}",
        orphan_tool_ids(msgs)
    );
    assert_valid_openai_pairing(msgs);

    // The surviving assistant keeps its real result and gains exactly one
    // `(aborted)` placeholder for the lost one, directly after itself.
    let idx = msgs
        .iter()
        .position(|m| matches!(m, Message::Assistant { tool_calls, .. } if tool_calls.len() == 2))
        .expect("the 2-call assistant message must survive compaction");
    assert!(
        matches!(
            &msgs[idx + 1],
            Message::Tool { tool_call_id, content }
                if tool_call_id == "call_a" && content == "tool output"
        ),
        "the real tool result must be kept verbatim next to its parent"
    );
    assert!(
        matches!(
            &msgs[idx + 2],
            Message::Tool { tool_call_id, content }
                if tool_call_id == "call_b" && content == ABORTED_TOOL_RESULT
        ),
        "the missing result must be synthesized as `{ABORTED_TOOL_RESULT}`"
    );

    // Budget behaviour is unchanged: compaction still lands on the 70% target.
    assert!(
        engine.token_count() <= compaction_target(400),
        "compact should bring transcript to <= 70% (got {})",
        engine.token_count()
    );
}

/// Regression (H1): pinning must not bypass the invariant. A dangling assistant
/// inside the pinned window (`messages[2..=rebirth checkpoint]`) is repaired too.
#[test]
fn test_compaction_pinned_prefix_respects_pairing_invariant() {
    let mut engine = ContextEngine::new(400);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());

    // Dangling assistant placed INSIDE the pinned prefix, followed by a rebirth
    // checkpoint which extends the pinned window past it.
    engine.append(assistant_with_tool_calls(&[
        "call_pinned_a",
        "call_pinned_b",
    ]));
    engine.append(tool_response("call_pinned_a"));
    engine.append(Message::System {
        content: format!("{REBIRTH_CHECKPOINT_PREFIX}checkpointed state)"),
    });

    push_over_trigger(&mut engine, 14);

    assert!(engine.should_compact(), "transcript must exceed 90% budget");
    engine.compact();

    let msgs = engine.messages();
    assert!(matches!(msgs[0], Message::System { .. }));
    assert!(matches!(msgs[1], Message::User { .. }));
    assert!(
        msgs.iter().any(|m| matches!(
            m,
            Message::System { content } if content.starts_with(REBIRTH_CHECKPOINT_PREFIX)
        )),
        "the rebirth checkpoint must still be pinned"
    );
    assert_valid_openai_pairing(msgs);
}

/// Helper level (REQ-CORE-003, M13): `prune_orphan_tool_messages` must remove
/// **only** the results whose owning assistant is gone, and must keep every
/// result that still has a parent — including the newest message of the
/// transcript. The input is the pair-split state the retention window leaves
/// behind: the assistant owning `call_dropped_parent` was dropped, while
/// `call_newest` (the newest message) is a legitimate result.
///
/// The expectation is a full-vector comparison, so the test fails for all three
/// wrong implementations: no pruning at all, pruning that drops the newest tool
/// result, or pruning that also removes results whose parent survived.
#[test]
fn test_prune_orphan_tool_messages_drops_only_results_without_a_surviving_parent() {
    let after_window = vec![
        Message::System {
            content: "sys".to_string(),
        },
        Message::User {
            content: "goal".to_string(),
        },
        assistant_with_tool_calls(&["call_surviving"]),
        tool_response("call_surviving"),
        // Its owner fell out of the retention window: this is the orphan.
        tool_response("call_dropped_parent"),
        assistant_with_tool_calls(&["call_newest"]),
        // The NEWEST message is a legitimate result and must survive.
        tool_response("call_newest"),
    ];
    let expected = vec![
        Message::System {
            content: "sys".to_string(),
        },
        Message::User {
            content: "goal".to_string(),
        },
        assistant_with_tool_calls(&["call_surviving"]),
        tool_response("call_surviving"),
        assistant_with_tool_calls(&["call_newest"]),
        tool_response("call_newest"),
    ];

    let out = prune_orphan_tool_messages(after_window.clone());

    assert_eq!(
        render(&out),
        render(&expected),
        "only the result whose assistant was dropped may be removed"
    );
    assert_eq!(
        tool_ids(&out),
        vec!["call_surviving".to_string(), "call_newest".to_string()],
        "every result with a surviving parent must be kept"
    );
    assert!(
        !tool_ids(&out).iter().any(|id| id == "call_dropped_parent"),
        "the orphan must be pruned"
    );
    assert!(
        matches!(
            out.last(),
            Some(Message::Tool { tool_call_id, .. }) if tool_call_id == "call_newest"
        ),
        "the newest tool result must be preserved, not pruned with the orphan"
    );
    // Non-tool messages are never touched by this pass.
    assert_eq!(
        out.iter()
            .filter(|m| matches!(m, Message::System { .. } | Message::User { .. }))
            .count(),
        2,
        "the pinned system/goal messages must survive untouched"
    );
    // What the pass removed is exactly the one orphan.
    assert_eq!(out.len(), after_window.len() - 1);
    assert_valid_openai_pairing(&out);
}

/// Canonical, comparable rendering of a transcript (`Message` has no `PartialEq`).
fn render(messages: &[Message]) -> Vec<String> {
    messages
        .iter()
        .map(|m| match m {
            Message::System { content } => format!("system:{content}"),
            Message::User { content } => format!("user:{content}"),
            Message::Assistant {
                content,
                reasoning_content,
                tool_calls,
            } => format!(
                "assistant:{}|{}|{}",
                content.clone().unwrap_or_default(),
                reasoning_content.clone().unwrap_or_default(),
                tool_calls
                    .iter()
                    .map(|tc| tc.id.clone())
                    .collect::<Vec<_>>()
                    .join(",")
            ),
            Message::Tool {
                tool_call_id,
                content,
            } => format!("tool:{tool_call_id}:{content}"),
        })
        .collect()
}

/// Helper level: `repair_tool_call_pairs` is a no-op on an already valid
/// transcript (real results kept verbatim), is idempotent, groups results under
/// their parent assistant even when other roles interleave them, synthesizes
/// exactly the `(aborted)` placeholder for missing results, and drops duplicate
/// results for the same call id.
#[test]
fn test_repair_tool_call_pairs_helper() {
    let valid = vec![
        Message::System {
            content: "sys".to_string(),
        },
        Message::User {
            content: "goal".to_string(),
        },
        assistant_with_tool_calls(&["c1", "c2"]),
        Message::Tool {
            tool_call_id: "c1".to_string(),
            content: "first result payload".to_string(),
        },
        Message::Tool {
            tool_call_id: "c2".to_string(),
            content: "second result payload".to_string(),
        },
        Message::Assistant {
            content: Some("done".to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        },
    ];

    let once = repair_tool_call_pairs(valid.clone());
    assert_valid_openai_pairing(&once);
    assert_eq!(
        render(&once),
        render(&valid),
        "an already valid transcript must be left untouched"
    );
    let twice = repair_tool_call_pairs(once.clone());
    assert_eq!(render(&twice), render(&once), "repair must be idempotent");

    // Results that were interleaved with other roles are regrouped under their
    // parent assistant, in `tool_calls` order.
    let interleaved = vec![
        Message::System {
            content: "sys".to_string(),
        },
        Message::User {
            content: "goal".to_string(),
        },
        Message::Tool {
            tool_call_id: "c2".to_string(),
            content: "second result payload".to_string(),
        },
        assistant_with_tool_calls(&["c1", "c2"]),
        Message::User {
            content: "steer".to_string(),
        },
        Message::Tool {
            tool_call_id: "c1".to_string(),
            content: "first result payload".to_string(),
        },
    ];
    let regrouped = repair_tool_call_pairs(interleaved);
    assert_valid_openai_pairing(&regrouped);
    let expected_regrouped = vec![
        Message::System {
            content: "sys".to_string(),
        },
        Message::User {
            content: "goal".to_string(),
        },
        assistant_with_tool_calls(&["c1", "c2"]),
        Message::Tool {
            tool_call_id: "c1".to_string(),
            content: "first result payload".to_string(),
        },
        Message::Tool {
            tool_call_id: "c2".to_string(),
            content: "second result payload".to_string(),
        },
        Message::User {
            content: "steer".to_string(),
        },
    ];
    assert_eq!(
        render(&regrouped),
        render(&expected_regrouped),
        "tool results must be re-emitted directly under their parent assistant"
    );

    // Dangling + orphan + duplicate shapes in one pass.
    let broken = vec![
        Message::System {
            content: "sys".to_string(),
        },
        Message::User {
            content: "goal".to_string(),
        },
        assistant_with_tool_calls(&["d1", "d2"]),
        tool_response("d1"),
        tool_response("d1"), // duplicate result for the same id
        tool_response("no-parent"),
    ];
    let repaired = repair_tool_call_pairs(broken);
    assert_valid_openai_pairing(&repaired);
    assert_eq!(
        render(&repaired),
        render(&[
            Message::System {
                content: "sys".to_string(),
            },
            Message::User {
                content: "goal".to_string(),
            },
            assistant_with_tool_calls(&["d1", "d2"]),
            tool_response_dup_content("d1", "tool output"),
            Message::Tool {
                tool_call_id: "d2".to_string(),
                content: ABORTED_TOOL_RESULT.to_string(),
            },
        ]),
        "duplicate/orphan results dropped, missing result synthesized"
    );
}

/// Build a tool result with explicit content.
fn tool_response_dup_content(id: &str, content: &str) -> Message {
    Message::Tool {
        tool_call_id: id.to_string(),
        content: content.to_string(),
    }
}

/// Helper level: for arbitrary histories (deterministic pseudo-random mix of
/// valid pairs, dangling `tool_calls`, orphan `Tool` messages, rebirth
/// checkpoints and filler), after `compact()` there are zero dangling
/// `tool_calls` and zero orphan `Tool` messages, the pinned prefix is intact,
/// and the budget target still holds.
#[test]
fn test_compaction_pairing_invariant_for_arbitrary_histories() {
    // Deterministic pseudo-random generator (fixed seed => reproducible run).
    let mut state: u64 = 0x2545_F491_4F6C_DD1D;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };

    for case in 0..120 {
        let budget = [260, 420, 900][case % 3];
        let mut engine = ContextEngine::new(budget);
        engine.set_system_prompt("You are a helpful coding assistant.".to_string());
        engine.set_goal("Refactor the parser module.".to_string());

        for counter in 0..(6 + case % 12) {
            match next() % 7 {
                0 | 1 => {
                    // plain turn
                    engine.append(Message::User {
                        content: format!(
                            "Verbose instruction number {case} step {counter} with padding words."
                        ),
                    });
                    engine.append(Message::Assistant {
                        content: Some(format!(
                            "Verbose assistant reply {counter} with padding words."
                        )),
                        reasoning_content: None,
                        tool_calls: vec![],
                    });
                }
                2 | 3 => {
                    // assistant + all of its results
                    let ids: Vec<String> = (0..1 + (counter % 3))
                        .map(|k| format!("call_{case}_{counter}_{k}"))
                        .collect();
                    engine.append(assistant_with_tool_calls(
                        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
                    ));
                    for id in &ids {
                        engine.append(Message::Tool {
                            tool_call_id: id.clone(),
                            content: format!("result payload for {id} with several words"),
                        });
                    }
                }
                4 => {
                    // dangling: only half of the results ever arrived
                    let ids: Vec<String> = (0..1 + (counter % 3))
                        .map(|k| format!("call_{case}_{counter}_{k}"))
                        .collect();
                    engine.append(assistant_with_tool_calls(
                        &ids.iter().map(String::as_str).collect::<Vec<_>>(),
                    ));
                    for id in ids.iter().take(ids.len() / 2) {
                        engine.append(tool_response(id));
                    }
                }
                5 => {
                    // orphan result with no assistant parent at all
                    engine.append(tool_response(&format!("orphan_{case}_{counter}")));
                }
                _ => {
                    // rebirth checkpoint in the middle of the transcript
                    engine.append(Message::System {
                        content: format!("{REBIRTH_CHECKPOINT_PREFIX}checkpoint {case})"),
                    });
                }
            }
        }

        // `compact()` must hold the invariant whether or not the window trims.
        engine.compact();

        let msgs = engine.messages();
        assert!(
            matches!(msgs[0], Message::System { .. }),
            "case {case}: messages[0] must stay the system prompt"
        );
        assert!(
            matches!(msgs[1], Message::User { .. }),
            "case {case}: messages[1] must stay the pinned goal"
        );
        assert_eq!(
            msgs[1].content().unwrap(),
            "Refactor the parser module.",
            "case {case}: pinned goal must be unchanged"
        );
        assert_eq!(
            dangling_tool_call_ids(msgs),
            Vec::<String>::new(),
            "case {case}: dangling tool_calls survived compact()"
        );
        assert_eq!(
            orphan_tool_ids(msgs),
            Vec::<String>::new(),
            "case {case}: orphan Tool messages survived compact()"
        );
        assert_valid_openai_pairing(msgs);
    }
}

/// The invariant must also survive a transcript that is already over the pinned
/// window and contains ONLY broken tool turns (worst case: every surviving
/// assistant is dangling, so the placeholder synthesis pushes the window over
/// the target and compaction must re-trim instead of overspending the budget).
#[test]
fn test_compaction_budget_target_holds_after_placeholder_synthesis() {
    let budget = 300;
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());

    push_over_trigger(&mut engine, 12);
    for i in 0..6 {
        let ids: Vec<String> = (0..3).map(|k| format!("many_{i}_{k}")).collect();
        engine.append(assistant_with_tool_calls(
            &ids.iter().map(String::as_str).collect::<Vec<_>>(),
        ));
    }

    assert!(engine.should_compact(), "transcript must exceed 90% budget");
    engine.compact();

    let msgs = engine.messages();
    assert!(
        engine.token_count() <= compaction_target(budget),
        "compact must still land on the 70% target after repairing (got {})",
        engine.token_count()
    );
    assert!(matches!(msgs[0], Message::System { .. }));
    assert!(matches!(msgs[1], Message::User { .. }));
    assert_valid_openai_pairing(msgs);
}

// ---------------------------------------------------------------------------
// M5 / M5b — compaction must be target-safe and must account truthfully
// (`docs/recon_bugs_manager.md`, items M5 + M5b).
//
// M5: the pinned window (`messages[0]`/`messages[1]` plus a pinned
// `REBIRTH CHECKPOINT`, see `pinned_prefix_end`) is charged against the 70%
// budget. If it alone exceeds the target, the target is unreachable — the pin
// is sacrosanct (REQ-CORE-001/002) — so `compact()` must report
// `CompactionOutcome::TargetUnreachable` instead of reporting success while
// delivering an over-budget transcript.
//
// RED-before evidence (before the fix `compact()` returned `()` and logged
// "Context compaction executed (automatic)" i.e. success):
//   pinned prefix = 103 tokens, target = 70 (budget 100)
//   after `compact()`: token_count = 103 > 70, still `should_compact() == true`
//
// M5b: the reclaimed counts are measured on the FINAL vector.
// RED-before evidence: the pre-pruning arithmetic
// `initial_len - (prefix_end + kept_tail)` (git HEAD `compact_to_target`,
// context.rs:530-539) reported `0` for a pruning-only pass in which the final
// vector actually dropped 1 message.
// ---------------------------------------------------------------------------

/// System prompt long enough that the pinned prefix alone exceeds the target.
const OVER_TARGET_SYSTEM_PROMPT: &str = "You are a meticulous engineering agent. You must read files before editing them. You must verify every change with the available test commands. Never guess at file contents. Always report findings precisely with file paths and line numbers. You are a meticulous engineering agent. You must read files before editing them. You must verify every change with the available test commands. Never guess at file contents. Always report findings precisely with file paths and line numbers. ";

/// The pinned-prefix cost of a transcript (the same window `compact()` pins).
fn pinned_tokens_of(messages: &[Message]) -> usize {
    let end = pinned_prefix_end(messages).min(messages.len());
    count_tokens(&messages[..end])
}

/// The pre-pruning window arithmetic that the old `compact_to_target` reported
/// as `removed` (git HEAD `src/manager/context.rs:518-539`), replicated here to
/// prove the reported number is stale.
fn stale_removed_before_pruning(messages: &[Message], target: usize) -> usize {
    let end = pinned_prefix_end(messages).min(messages.len());
    let mut total = count_tokens(&messages[..end]);
    let mut kept_tail = 0usize;
    for m in messages[end..].iter().rev() {
        let cost = count_tokens(std::slice::from_ref(m));
        if total + cost > target && kept_tail > 0 {
            break;
        }
        total += cost;
        kept_tail += 1;
    }
    messages.len() - (end + kept_tail)
}

#[test]
fn test_compaction_reports_failure_when_pinned_prefix_alone_exceeds_target() {
    let budget = 100usize;
    let target = compaction_target(budget); // 0.70 * 100 = 70
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt(OVER_TARGET_SYSTEM_PROMPT.to_string());
    engine.set_goal("Refactor the parser module across every call site.".to_string());
    push_over_trigger(&mut engine, 8);
    assert!(engine.should_compact(), "fixture must be over the trigger");

    let pinned = pinned_tokens_of(engine.messages());
    assert!(
        pinned > target,
        "fixture: the pinned prefix ({pinned}) must exceed the target ({target})"
    );

    let outcome = engine.compact();

    // M5: a distinct, visible failure — never a silent success.
    assert!(
        !outcome.succeeded(),
        "compaction must not report success when the target is unreachable"
    );
    match outcome {
        CompactionOutcome::TargetUnreachable {
            pinned_tokens,
            target: reported_target,
            ..
        } => {
            assert_eq!(reported_target, target);
            assert_eq!(
                pinned_tokens, pinned,
                "the failure must name the un-budgeted pinned prefix as the cause"
            );
        }
        other => panic!("expected TargetUnreachable, got {other:?}"),
    }
    assert_eq!(outcome.target(), target);
    assert_eq!(outcome.final_tokens(), engine.token_count());
    assert!(
        engine.token_count() > target,
        "the transcript really is over budget, which is exactly why this must fail"
    );

    // REQ-CORE-001/002: the failure must not have eaten the pin.
    assert!(matches!(engine.messages()[0], Message::System { .. }));
    assert_eq!(
        engine.messages()[1].content().unwrap_or_default(),
        "Refactor the parser module across every call site."
    );
    // Everything removable was still trimmed, and the pairing invariant holds.
    assert!(
        !engine.messages()[..]
            .iter()
            .any(|m| matches!(m, Message::Assistant { .. } | Message::Tool { .. })),
        "every non-pinned turn is trimmed even on the failure path"
    );
    assert_valid_openai_pairing(engine.messages());

    // The failure is stable/terminating: re-compacting cannot remove more.
    let again = engine.compact();
    assert!(
        matches!(again, CompactionOutcome::TargetUnreachable { .. }),
        "a second compaction attempt must also report failure, not success"
    );
    assert_eq!(again.messages_removed(), 0);
    assert_eq!(again.final_tokens(), engine.token_count());
}

#[test]
fn test_compaction_budgets_pinned_rebirth_checkpoint_and_reports_failure() {
    // The checkpoint pinned at `messages[2]` is now part of the arithmetic
    // (M5): a long checkpoint can make the target unreachable on its own.
    let budget = 300usize;
    let target = compaction_target(budget); // 210
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("System prompt for coding agent.".to_string());
    engine.set_goal("Initial user goal for refactoring.".to_string());

    let plain_pins = pinned_tokens_of(engine.messages());
    assert!(
        plain_pins <= target,
        "fixture: [0]+[1] alone must fit the target (got {plain_pins}/{target})"
    );

    let long_summary = "Inspected parser.rs and mapped every call site. ".repeat(30);
    engine.perform_rebirth(&long_summary);
    // [0] system, [1] goal, [2] checkpoint — no duplicated goal (M12).
    assert_eq!(engine.messages().len(), 3);

    let pinned = pinned_tokens_of(engine.messages());
    assert!(
        pinned > target,
        "fixture: the pinned checkpoint window ({pinned}) must exceed the target ({target})"
    );
    push_over_trigger(&mut engine, 6);

    let outcome = engine.compact();
    assert!(
        matches!(outcome, CompactionOutcome::TargetUnreachable { .. }),
        "a checkpoint window over the target must fail, got {outcome:?}"
    );
    match outcome {
        CompactionOutcome::TargetUnreachable {
            pinned_tokens,
            pinned_messages,
            ..
        } => {
            assert_eq!(pinned_messages, 3, "the checkpoint is part of the pin");
            assert_eq!(pinned_tokens, pinned);
        }
        other => panic!("expected TargetUnreachable, got {other:?}"),
    }
    // The checkpoint itself is never dropped by the failure path.
    assert!(
        engine.messages().iter().any(|m| m
            .content()
            .unwrap_or_default()
            .starts_with(REBIRTH_CHECKPOINT_PREFIX)),
        "the pinned checkpoint must survive"
    );
    assert_valid_openai_pairing(engine.messages());
}

#[test]
fn test_compaction_reports_success_and_hits_target_for_reachable_targets() {
    // 0.70 semantics are unchanged for reachable targets (M5 must not move the
    // ratio constants nor make every compaction fail).
    let budget = 300usize;
    let target = compaction_target(budget);
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());
    push_over_trigger(&mut engine, 12);
    assert!(engine.should_compact());

    let before_msgs = engine.messages().len();
    let before_tokens = engine.token_count();
    let outcome = engine.compact();

    assert!(
        outcome.succeeded(),
        "a reachable target must still succeed: {outcome:?}"
    );
    assert!(matches!(outcome, CompactionOutcome::Compacted { .. }));
    assert_eq!(outcome.target(), target);
    assert!(
        outcome.final_tokens() <= target,
        "0.70 target still honoured"
    );
    assert_eq!(outcome.final_tokens(), engine.token_count());
    assert_eq!(
        outcome.messages_removed(),
        before_msgs - engine.messages().len()
    );
    assert_eq!(
        outcome.tokens_reclaimed(),
        before_tokens - engine.token_count()
    );
    assert!(!engine.should_compact(), "must land back under the trigger");
}

#[test]
fn test_compaction_removed_count_is_the_final_vector_delta_for_pruning_only_passes() {
    let budget = 400usize;
    let target = compaction_target(budget);
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());
    engine.append(Message::User {
        content: "Deploy the parser changes and run the test suite.".to_string(),
    });
    engine.append(Message::Assistant {
        content: Some("Running the suite now.".to_string()),
        reasoning_content: None,
        tool_calls: vec![],
    });
    // An orphan result sitting INSIDE the retention window: only the pairing
    // repair (pruning) can ever remove it.
    engine.append(tool_response("call_stray"));

    let original = engine.messages().to_vec();
    let stale = stale_removed_before_pruning(&original, target);
    assert_eq!(
        stale, 0,
        "fixture: the retention window alone drops nothing (the M5b blind spot)"
    );

    let outcome = engine.compact();
    let true_removed = original.len() - engine.messages().len();

    assert!(outcome.succeeded());
    assert_eq!(
        outcome.messages_removed(),
        true_removed,
        "M5b: the reported count must be the delta of the FINAL vector"
    );
    assert_eq!(
        outcome.messages_removed(),
        1,
        "the pruned orphan must be counted as removed"
    );
    assert!(
        !engine.messages().iter().any(|m| matches!(
            m,
            Message::Tool { tool_call_id, .. } if tool_call_id == "call_stray"
        )),
        "the orphan result must be gone from the final vector"
    );
    assert!(
        engine.token_count() <= target,
        "the compacted transcript must still land on the 70% target"
    );
    assert_valid_openai_pairing(engine.messages());
}

#[test]
fn test_compaction_removed_count_covers_pruning_beyond_the_retention_window() {
    let budget = 300usize;
    let target = compaction_target(budget);
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());

    // Old turn that the retention window is expected to drop ...
    engine.append(assistant_with_tool_calls(&["call_old_1", "call_old_2"]));
    engine.append(tool_response("call_old_1"));
    engine.append(tool_response("call_old_2"));
    push_over_trigger(&mut engine, 8);
    // ... plus an orphan result as the NEWEST message: it is always inside the
    // window, so it is removed by pruning only.
    engine.append(tool_response("call_stray"));

    let original = engine.messages().to_vec();
    let stale = stale_removed_before_pruning(&original, target);

    let outcome = engine.compact();
    let true_removed = original.len() - engine.messages().len();

    assert!(outcome.succeeded(), "target is reachable here: {outcome:?}");
    assert_eq!(outcome.messages_removed(), true_removed);
    assert_eq!(
        outcome.messages_removed(),
        stale + 1,
        "M5b: pruning removes one message more than the window arithmetic counted \
         (stale {stale}, true {true_removed})"
    );
    assert_eq!(
        outcome.tokens_reclaimed(),
        count_tokens(&original) - engine.token_count()
    );
    assert_valid_openai_pairing(engine.messages());
}

// ---------------------------------------------------------------------------
// M6 / M12 — `perform_rebirth()` must summarize the real history, not the
// runtime's own injected boilerplate, must not emit a second `system` turn, and
// must not duplicate the pinned goal (`docs/recon_bugs_manager.md`).
//
// RED-before evidence (pre-fix `perform_rebirth`):
//   messages[2] == "(SYSTEM: Rebirth checkpoint accepted. Proceed
//   immediately.)" (an injected continuation notice, not the real instruction);
//   the checkpoint was `Message::System` -> `role() == "system"`;
//   with no distinct instruction the goal was emitted verbatim at [1] AND [2]
//   (2 copies).
// ---------------------------------------------------------------------------

#[test]
fn test_rebirth_slot2_excludes_injected_advisory_messages() {
    let mut engine = ContextEngine::new(600);
    engine.set_system_prompt("System prompt.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());
    engine.append(Message::User {
        content: "Genuine last instruction: run the integration tests.".to_string(),
    });
    engine.append(Message::Assistant {
        content: Some("Working on it.".to_string()),
        reasoning_content: None,
        tool_calls: vec![],
    });
    // Every shape of runtime-injected `User` boilerplate that used to win the
    // `messages[2]` slot because it was the newest non-goal user message. The
    // fixtures are spelled from the single owner of those notices,
    // [`INJECTED_ADVISORY_PREFIXES`], so the grammar cannot drift apart here.
    let legacy_limit_notice = INJECTED_ADVISORY_PREFIXES[1];
    let legacy_continuation_notice = INJECTED_ADVISORY_PREFIXES[2];
    engine.inject_rebirth_advisory();
    engine.append(Message::User {
        content: format!("{legacy_limit_notice}. Summarize and invoke rebirth.)"),
    });
    engine.append(Message::User {
        content: format!("{legacy_continuation_notice}. Proceed immediately.)"),
    });

    engine.perform_rebirth("Inspected parser.rs at line 250; next step is ast.rs.");
    let msgs = engine.messages();

    assert_eq!(
        msgs[2].content().unwrap_or_default(),
        "Genuine last instruction: run the integration tests.",
        "M6: messages[2] must be the last GENUINE instruction"
    );
    // The first three prefixes in the owner's table are the injected notices; the
    // fourth is the checkpoint itself, which is of course present at `msgs[3]`.
    for m in msgs {
        let c = m.content().unwrap_or_default();
        for notice in INJECTED_ADVISORY_PREFIXES.iter().take(3) {
            assert!(
                !c.contains(notice),
                "M6: no injected advisory text ({notice}) may be summarized into the \
                 checkpoint state: {c}"
            );
        }
    }
    // The advisory flag machinery is untouched.
    assert!(!engine.rebirth_advisory_emitted());
    assert_valid_openai_pairing(msgs);
}

#[test]
fn test_rebirth_checkpoint_is_a_user_message_and_stays_pinned() {
    let mut engine = ContextEngine::new(400);
    engine.set_system_prompt("System prompt.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());
    engine.append(Message::User {
        content: "Run the integration test suite next.".to_string(),
    });

    engine.perform_rebirth("Summarized progress: read parser.rs.");
    let msgs = engine.messages();
    assert_eq!(msgs.len(), 4);
    let last = msgs.last().expect("a checkpoint must be emitted");
    assert_eq!(
        last.role(),
        "user",
        "M12: the REBIRTH CHECKPOINT must be a User turn, not a second system turn"
    );
    assert!(
        matches!(last, Message::User { .. }),
        "M12: expected a `Message::User` checkpoint"
    );
    assert!(
        last.content()
            .unwrap_or_default()
            .starts_with(REBIRTH_CHECKPOINT_PREFIX),
        "the REBIRTH CHECKPOINT prefix behaviour must be preserved"
    );

    // A later compaction must still pin it (the pin matches on the marker, not
    // on the historic `System` role).
    engine.reset_consecutive_rebirths();
    push_over_trigger(&mut engine, 14);
    assert!(engine.should_compact(), "must exceed the 90% trigger");
    let outcome = engine.compact();
    assert!(outcome.succeeded(), "target reachable: {outcome:?}");
    let pinned = pinned_prefix_end(engine.messages());
    assert!(
        engine.messages()[..pinned].iter().any(|m| m
            .content()
            .unwrap_or_default()
            .starts_with(REBIRTH_CHECKPOINT_PREFIX)),
        "the rebirth checkpoint must stay pinned after rebirth"
    );
    assert_valid_openai_pairing(engine.messages());
}

#[test]
fn test_rebirth_does_not_duplicate_the_pinned_goal() {
    const GOAL: &str = "Refactor the parser module.";

    // Case A: no distinct instruction after the goal.
    let mut engine = ContextEngine::new(600);
    engine.set_system_prompt("System prompt.".to_string());
    engine.set_goal(GOAL.to_string());
    engine.append(Message::Assistant {
        content: Some("Thinking about the parser.".to_string()),
        reasoning_content: None,
        tool_calls: vec![],
    });

    engine.perform_rebirth("Summarized progress: read parser.rs.");
    let msgs = engine.messages();
    let occurrences = msgs
        .iter()
        .filter(|m| m.content().unwrap_or_default() == GOAL)
        .count();
    assert_eq!(
        occurrences, 1,
        "M12: the pinned goal must appear exactly once, not be duplicated at [2]"
    );
    assert_eq!(msgs.len(), 3, "no distinct instruction -> 3 messages");
    assert!(matches!(&msgs[0], Message::System { content } if content == "System prompt."));
    assert_eq!(msgs[1].content().unwrap_or_default(), GOAL);
    assert!(
        msgs[2]
            .content()
            .unwrap_or_default()
            .starts_with(REBIRTH_CHECKPOINT_PREFIX),
        "the checkpoint follows the pinned goal directly"
    );
    // t-031a invariant must hold after rebirth too.
    assert_valid_openai_pairing(msgs);

    // Case B: a distinct instruction keeps the 4-message shape, and the goal is
    // still stated exactly once (the checkpoint text must not restate it).
    let mut engine2 = ContextEngine::new(600);
    engine2.set_system_prompt("System prompt.".to_string());
    engine2.set_goal(GOAL.to_string());
    engine2.append(Message::User {
        content: "Now split the tokenizer out into its own module.".to_string(),
    });
    engine2.perform_rebirth("Split started; tokenizer.rs created.");
    let msgs2 = engine2.messages();
    assert_eq!(msgs2.len(), 4);
    assert_eq!(
        msgs2[2].content().unwrap_or_default(),
        "Now split the tokenizer out into its own module."
    );
    assert_ne!(msgs2[2].content().unwrap_or_default(), GOAL);
    let occurrences = msgs2
        .iter()
        .filter(|m| m.content().unwrap_or_default() == GOAL)
        .count();
    assert_eq!(
        occurrences, 1,
        "the goal appears exactly once in the 4-message shape too"
    );
    let checkpoint = msgs2[3].content().unwrap_or_default();
    assert!(
        !checkpoint.contains(GOAL),
        "the checkpoint text must not restate the goal"
    );
    assert_valid_openai_pairing(msgs2);
}

// ─────────────────────────────────────────────────────────────────────────────
// M7 — the context budget model must account for the tool schemas that ride on
// every request (`ChatRequest.tools`), not only for the message array.
// ─────────────────────────────────────────────────────────────────────────────

/// Filler turn used to grow a transcript to a target token window.
const M7_FILLER: &str = "Verbose manager turn padding the transcript with plenty of tokens.";

/// Grow an engine's transcript while the *transcript alone* stays at or below
/// the 90% compaction trigger.
fn m7_fill_below_trigger(engine: &mut ContextEngine, schema_tokens: usize) {
    let threshold = compaction_threshold(engine.max_context_tokens());
    while engine.token_count() + schema_tokens <= threshold {
        engine.append(Message::User {
            content: M7_FILLER.to_string(),
        });
        engine.append(Message::Assistant {
            content: Some(M7_FILLER.to_string()),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
}

#[test]
fn test_m7_tools_tokens_counts_the_schemas_actually_sent() {
    use crate::types::ToolDef;

    // Additive/optional contract: no schemas means no extra term at all.
    assert_eq!(tools_tokens(&[]), 0, "an empty tool list must add zero");

    // The pinned name set must mirror the real Manager wire set exactly
    // (consumed from crate::tool_names so a typo cannot drift in).
    let manager = ToolDef::manager_tools();
    let mut wire: Vec<String> = manager.iter().map(|t| t.function.name.clone()).collect();
    let mut pinned: Vec<String> = MANAGER_TOOL_NAMES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    wire.sort();
    pinned.sort();
    assert_eq!(
        wire, pinned,
        "MANAGER_TOOL_NAMES must stay in lockstep with ToolDef::manager_tools()"
    );

    let manager_tokens = tools_tokens(&manager);
    let specialist_tokens = tools_tokens(&ToolDef::default_tools());
    assert!(
        manager_tokens > 0,
        "the manager schema payload must be charged, got {manager_tokens}"
    );
    assert!(
        specialist_tokens > manager_tokens,
        "the 18-tool specialist payload ({specialist_tokens}) must cost more than the \
         9-tool manager payload ({manager_tokens})"
    );
    // Every definition is charged with its framing constant plus its body.
    assert!(
        manager_tokens >= manager.len() * TOOL_DEF_FRAMING_TOKENS,
        "per-definition framing must be charged"
    );
    // Deterministic across calls (the value is cached by the engine).
    assert_eq!(manager_tokens, tools_tokens(&manager));

    // A single definition's cost is bounded by its own serialized body: renaming
    // a tool to a longer name must increase the count.
    let mut renamed = manager.clone();
    let original = tools_tokens(&renamed);
    renamed[0].function.name = format!("{}_", renamed[0].function.name);
    assert!(
        tools_tokens(&renamed) > original,
        "the tool name must be part of the counted payload"
    );
}

#[test]
fn test_m7_request_tokens_is_transcript_plus_schemas() {
    use crate::types::ToolDef;

    let messages = vec![
        Message::System {
            content: "You are the manager.".to_string(),
        },
        Message::User {
            content: "Build a rocket.".to_string(),
        },
    ];
    // Existing numeric semantics of count_tokens are untouched.
    assert_eq!(
        request_tokens(&messages, &[]),
        count_tokens(&messages),
        "callers that pass no tools behave exactly as before"
    );
    let tools = ToolDef::manager_tools();
    assert_eq!(
        request_tokens(&messages, &tools),
        count_tokens(&messages) + tools_tokens(&tools),
        "the new term is strictly additive"
    );
    assert!(
        request_tokens(&messages, &tools) > count_tokens(&messages),
        "the tool payload must never be zero for a real request"
    );
}

#[test]
fn test_m7_compaction_trigger_reflects_the_true_request_size() {
    use crate::types::ToolDef;

    let tools = ToolDef::manager_tools();
    let schema = tools_tokens(&tools);
    assert!(schema > 0);

    // A budget where the transcript alone never reaches the 90% trigger, but the
    // request (transcript + schemas) does.
    let budget = schema * 4;
    let mut engine = ContextEngine::new(budget);
    engine.set_system_prompt("You are the manager.".to_string());
    engine.set_goal("Build a rocket.".to_string());
    m7_fill_below_trigger(&mut engine, schema);

    assert!(
        engine.token_count() <= compaction_threshold(budget),
        "fixture: transcript alone ({}) must stay at or below the trigger ({})",
        engine.token_count(),
        compaction_threshold(budget)
    );
    assert!(
        !engine.should_compact(),
        "an engine that declared no tools keeps message-only accounting"
    );
    assert_eq!(engine.tool_schema_tokens(), 0);
    assert_eq!(engine.request_token_count(), engine.token_count());

    engine.set_tools(&tools);
    assert_eq!(engine.tool_schema_tokens(), schema);
    assert_eq!(engine.request_token_count(), engine.token_count() + schema);
    assert!(
        engine.should_compact(),
        "M7: transcript {} + schemas {} must cross the trigger {}, but the trigger \
         only saw the transcript",
        engine.token_count(),
        schema,
        compaction_threshold(budget)
    );
    // Explicit (non-mutating) variant agrees with the declared-tools variant.
    assert!(engine.should_compact_with_tools(&tools));
    assert!(!engine.should_compact_with_tools(&[]));

    // Declaring no tools again restores the pre-M7 decision exactly.
    engine.set_tools(&[]);
    assert_eq!(engine.tool_schema_tokens(), 0);
    assert!(
        !engine.should_compact(),
        "resetting the tool payload restores tool-free behaviour"
    );
}

#[test]
fn test_m7_manager_context_charges_its_default_tool_schemas() {
    use crate::types::ToolDef;

    let schema = tools_tokens(&ToolDef::manager_tools());
    let budget = schema * 4;
    // The Manager transcript is built by the factory, whose requests always send
    // ToolDef::manager_tools() (src/llm/stream.rs::build_request).
    let mut engine = ContextEngineFactory::new(budget).manager_context(
        "You are the manager.".to_string(),
        "Build a rocket.".to_string(),
    );
    m7_fill_below_trigger(&mut engine, schema);

    assert_eq!(
        engine.tool_schema_tokens(),
        schema,
        "the manager engine must charge the schemas its requests carry"
    );
    assert!(
        engine.token_count() <= compaction_threshold(budget),
        "fixture: transcript ({}) stays at or below the trigger ({})",
        engine.token_count(),
        compaction_threshold(budget)
    );
    assert!(
        engine.should_compact(),
        "M7: the request really is {} tokens ({} transcript + {} schemas) against a \
         trigger of {}",
        engine.request_token_count(),
        engine.token_count(),
        schema,
        compaction_threshold(budget)
    );
    // Specialists keep explicit accounting: their tool list is role-filtered.
    let specialist = ContextEngineFactory::new(budget).specialist_context(
        "You are a coder.".to_string(),
        "Build a rocket.".to_string(),
    );
    assert_eq!(
        specialist.tool_schema_tokens(),
        0,
        "specialist engines must not be charged a guessed tool set"
    );
}

/// (t-076) Doc-shape guard for the tool-schema accounting docs. The module doc and
/// the factory docs must name **every** live declarer of a tool-schema charge, so the
/// "message-only is a **lower bound**" caveat can never again be read as covering the
/// live paths. All three live declarers are pinned by name, as is the evidence file
/// proving the newest one is live, and the caveat must stay scoped to a caller that
/// declares nothing at all.
#[test]
fn tool_schema_docs_name_every_live_declarer_and_scope_the_lower_bound_caveat() {
    let src = include_str!("context.rs");
    // Doc comments only: `//!` (module) and `///` (items).
    let docs: String = src
        .lines()
        .filter(|line| {
            let t = line.trim_start();
            t.starts_with("//!") || t.starts_with("///")
        })
        .collect::<Vec<&str>>()
        .join("\n");
    assert!(
        docs.contains("lower bound"),
        "the docs must keep the message-only lower-bound caveat explicit"
    );
    for declarer in [
        // (1) Manager path.
        "build_manager_context",
        "sync_manager_tool_schema",
        // (2) fix loop.
        "charge_engine_tool_schema",
        // (3) specialist turn (gate t-073).
        "build_specialist_context",
        "sync_specialist_tool_schema",
    ] {
        assert!(
            docs.contains(declarer),
            "the docs must name the live tool-schema declarer `{declarer}` \
             (an un-named declarer makes the lower-bound caveat misleading)"
        );
    }
    assert!(
        docs.contains("test_specialist_schema_charge"),
        "the docs must cite the test that proves the specialist charge is live"
    );
    // The caveat must stay scoped: an uncharged engine is only what a caller that
    // declares nothing ends up with.
    for qualifier in ["declares nothing at all", "never goes through"] {
        assert!(
            docs.contains(qualifier),
            "the lower-bound caveat must be scoped to an engine that never declares \
             its list (missing: {qualifier})"
        );
    }
}
