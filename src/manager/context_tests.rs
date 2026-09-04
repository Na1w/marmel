use super::*;
use crate::types::ToolCall;

/// Build an assistant message carrying one tool call with the given id.
fn assistant_with_tool(id: &str) -> Message {
    Message::Assistant {
        content: Some("let me call a tool".to_string()),
        reasoning_content: None,
        tool_calls: vec![ToolCall::new(id, "read_file", "{\"path\":\"x\"}")],
    }
}

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

#[test]
fn test_context_compaction_orphan_removal() {
    let mut engine = ContextEngine::new(120);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());

    // A valid assistant->tool pair, plus an orphaned tool message whose
    // assistant call has been dropped (simulating an earlier prune).
    engine.append(assistant_with_tool("call_abc"));
    engine.append(tool_response("call_abc"));
    engine.append(tool_response("call_orphan")); // no matching assistant

    // Force the transcript well over the trigger threshold.
    for i in 0..12 {
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

    assert!(
        engine.should_compact(),
        "transcript should exceed 90% budget"
    );

    engine.compact();

    let target = compaction_target(120);
    assert!(
        engine.token_count() <= target,
        "compact should bring transcript to <= 70% (got {})",
        engine.token_count()
    );

    // Pins survive.
    assert!(matches!(engine.messages()[0], Message::System { .. }));
    assert!(matches!(engine.messages()[1], Message::User { .. }));

    // No orphaned tool message survives.
    let tool_ids: Vec<String> = engine
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !tool_ids.iter().any(|id| id == "call_orphan"),
        "orphaned tool message must be removed"
    );
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
    assert_eq!(msgs.len(), 4, "rebirth must collapse to exactly 4 messages");

    // [0] system, [1] goal, [2] last user instruction, [3] checkpoint.
    assert!(matches!(msgs[0], Message::System { .. }));
    assert!(matches!(msgs[1], Message::User { .. }));
    match &msgs[2] {
        Message::User { content } => {
            assert_eq!(content, "Final instruction distinct from the goal.")
        }
        _ => panic!("messages[2] should be the last user instruction"),
    }
    match &msgs[3] {
        Message::System { content } => {
            assert!(
                content.starts_with(REBIRTH_CHECKPOINT_PREFIX),
                "messages[3] must be the REBIRTH CHECKPOINT injection"
            );
            assert!(content.contains("rewrote the module under test"));
        }
        _ => panic!("messages[3] should be the checkpoint system message"),
    }

    // The session_rebirths counter must be incremented.
    assert_eq!(
        stats
            .session_rebirths
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn test_context_utf8_safe_slicing() {
    // Multibyte chars (é, ö, €, 中, 文) must never be severed mid-byte
    // regardless of the requested byte offsets (REQ-CORE-006).
    let s = "héllo wörld € 中文";

    // Offsets landing mid-char must be clamped to char boundaries.
    let sliced = utf8_safe_slice(s, 2, 14);
    // `sliced` is produced by indexing at valid boundaries, so it must be
    // a self-consistent UTF-8 slice (no panic / no split char).
    let _ = sliced.chars().count();
    assert_eq!(sliced, std::str::from_utf8(sliced.as_bytes()).unwrap());
    // Every byte in the output must be part of whole chars — i.e. the
    // output must round-trip through char boundaries losslessly.
    assert_eq!(sliced.chars().collect::<String>(), sliced);

    // Out-of-range offsets must clamp rather than panic.
    assert_eq!(utf8_safe_slice(s, 40, 100), "");

    // Empty input is a no-op.
    assert_eq!(utf8_safe_slice("", 0, 0), "");
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

/// Build a transcript that is well over the given budget so compaction
/// actually removes messages.
fn fill_over_budget(max_tokens: usize) -> ContextEngine {
    let mut engine = ContextEngine::new(max_tokens);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());
    for i in 0..20 {
        engine.append(Message::User {
                content: format!(
                    "Turn {i}: please make the following verbose change to the file with lots of padding text to consume many tokens."
                ),
            });
        engine.append(Message::Assistant {
            content: Some(format!(
                "Understood, working on turn {i} now with a lengthy verbose reply full of detail."
            )),
            reasoning_content: None,
            tool_calls: vec![],
        });
    }
    engine
}

/// Build a transcript whose token count lands strictly between the 90%
/// trigger threshold and the hard `max_tokens` limit, so the retry-1 ratio
/// path (70%) is exercised rather than the over-limit 80% path.
fn fill_in_trigger_window(max_tokens: usize) -> ContextEngine {
    let mut engine = ContextEngine::new(max_tokens);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());
    let mut i = 0usize;
    loop {
        engine.append(Message::User {
                content: format!(
                    "Turn {i}: please make the following verbose change to the file with lots of padding text to consume many tokens."
                ),
            });
        engine.append(Message::Assistant {
            content: Some(format!(
                "Understood, working on turn {i} now with a lengthy verbose reply full of detail."
            )),
            reasoning_content: None,
            tool_calls: vec![],
        });
        i += 1;
        let n = engine.token_count();
        if n > compaction_threshold(max_tokens) {
            // Stop once we cross the 90% trigger; the transcript must stay
            // under the hard limit for the ratio path to apply.
            assert!(
                n <= max_tokens,
                "test transcript must stay under the hard limit (got {n} > {max_tokens})"
            );
            break;
        }
    }
    engine
}

/// Token count of the transcript excluding the injected
/// `SYSTEM: CONTEXT LIMIT EXCEEDED` message, so compaction targets can be
/// asserted independently of the post-compaction injection.
fn count_without_injection(engine: &ContextEngine) -> usize {
    count_tokens(
            &engine
                .messages()
                .iter()
                .filter(|m| {
                    !matches!(m, Message::User { content } if content == CONTEXT_LIMIT_EXCEEDED_MESSAGE)
                })
                .cloned()
                .collect::<Vec<_>>(),
        )
}

#[test]
fn test_compaction_retry_escalation_70_then_50() {
    // Budget 1000 gives a 100-token trigger window (90%..100%), wide enough
    // for the ~48-token per-turn increment to land inside it.
    let mut engine = fill_in_trigger_window(1000);
    let limit = engine.max_context_tokens();

    // Retry 1: ratio path targets 70% of the pre-compaction token count.
    let pre1 = engine.token_count();
    let compacted = engine.compact_with_retry(limit);
    assert!(compacted, "first retry must compact");
    assert_eq!(engine.compaction_retry_count(), 1);
    let target1 = ((pre1.min(limit) as f64) * COMPACTION_RETRY1_RATIO).round() as usize;
    assert!(
        count_without_injection(&engine) <= target1,
        "retry 1 should bring transcript to <= 70% of pre-compaction count (got {}, target {})",
        count_without_injection(&engine),
        target1
    );

    // The CONTEXT LIMIT EXCEEDED injection must be present after success.
    assert!(
        engine.messages().iter().any(
            |m| matches!(m, Message::User { content } if content == CONTEXT_LIMIT_EXCEEDED_MESSAGE)
        ),
        "CONTEXT LIMIT EXCEEDED injection must be appended after a successful compaction"
    );

    // Re-fill to force a second retry (back into the trigger window).
    let mut i = 0usize;
    while engine.token_count() <= compaction_threshold(limit) {
        engine.append(Message::User {
                content: format!(
                    "More turn {i}: please make the following verbose change to the file with lots of padding text to consume many tokens."
                ),
            });
        engine.append(Message::Assistant {
                content: Some(format!(
                    "Understood, working on more turn {i} now with a lengthy verbose reply full of detail."
                )),
                reasoning_content: None,
                tool_calls: vec![],
            });
        i += 1;
    }

    // Retry 2: ratio path targets 50% of the pre-compaction token count.
    let pre2 = engine.token_count();
    let compacted2 = engine.compact_with_retry(limit);
    assert!(compacted2, "second retry must compact");
    assert_eq!(engine.compaction_retry_count(), 2);
    let target2 = ((pre2.min(limit) as f64) * COMPACTION_RETRY2_RATIO).round() as usize;
    assert!(
        count_without_injection(&engine) <= target2,
        "retry 2 should bring transcript to <= 50% of pre-compaction count (got {}, target {})",
        count_without_injection(&engine),
        target2
    );

    // Third attempt is gated by `compaction_retry_count < 2`.
    let compacted3 = engine.compact_with_retry(limit);
    assert!(!compacted3, "retry cap reached: no further compaction");
    assert_eq!(engine.compaction_retry_count(), 2);

    // Reset restores the counter.
    engine.reset_compaction_retry_count();
    assert_eq!(engine.compaction_retry_count(), 0);
}

#[test]
fn test_compaction_retry1_over_limit_uses_80_percent() {
    // When retry 1 finds the transcript over the hard limit, it uses the
    // 80%-of-limit target (caesar `compact_context`).
    let mut engine = fill_over_budget(200);
    let limit = engine.max_context_tokens();
    assert!(
        engine.token_count() > limit,
        "test transcript must exceed the hard limit"
    );

    let compacted = engine.compact_with_retry(limit);
    assert!(compacted);
    assert_eq!(engine.compaction_retry_count(), 1);
    let target = ((limit as f64) * COMPACTION_OVER_LIMIT_TARGET_RATIO).round() as usize;
    assert!(
        count_without_injection(&engine) <= target,
        "retry 1 over-limit should bring transcript to <= 80% of limit (got {}, target {})",
        count_without_injection(&engine),
        target
    );
}

#[test]
fn test_compaction_retry_prunes_orphan_tools() {
    let mut engine = ContextEngine::new(120);
    engine.set_system_prompt("You are a helpful coding assistant.".to_string());
    engine.set_goal("Refactor the parser module.".to_string());

    // A valid assistant->tool pair, plus an orphaned tool message whose
    // assistant call will be dropped during compaction.
    engine.append(assistant_with_tool("call_abc"));
    engine.append(tool_response("call_abc"));
    engine.append(tool_response("call_orphan")); // no matching assistant

    // Force the transcript well over the trigger threshold.
    for i in 0..12 {
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

    assert!(
        engine.should_compact(),
        "transcript should exceed 90% budget"
    );

    let compacted = engine.compact_with_retry(engine.max_context_tokens());
    assert!(compacted, "retry compaction must succeed");

    // No orphaned tool message survives.
    let tool_ids: Vec<String> = engine
        .messages()
        .iter()
        .filter_map(|m| match m {
            Message::Tool { tool_call_id, .. } => Some(tool_call_id.clone()),
            _ => None,
        })
        .collect();
    assert!(
        !tool_ids.iter().any(|id| id == "call_orphan"),
        "orphaned tool message must be removed"
    );

    // Pins survive.
    assert!(matches!(engine.messages()[0], Message::System { .. }));
    assert!(matches!(engine.messages()[1], Message::User { .. }));
}

#[test]
fn test_compaction_retry_cap_blocks_third_attempt() {
    let mut engine = fill_over_budget(200);
    let limit = engine.max_context_tokens();

    // Two successful retries consume the cap.
    assert!(engine.compact_with_retry(limit));
    assert!(engine.compact_with_retry(limit));
    assert_eq!(engine.compaction_retry_count(), 2);

    // A third attempt is refused without mutating the transcript.
    let before = engine.messages().len();
    assert!(!engine.compact_with_retry(limit));
    assert_eq!(engine.messages().len(), before);
}

#[test]
fn test_slow_prefill_threshold_is_inclusive_at_300s() {
    // REQ-CORE-005 / caesar `record_backend_duration`: a prefill of exactly
    // 300s is a slow prefill (`duration_secs >= 300`).
    let mut t = SlowPrefillTracker::new();
    // Two consecutive slow prefills at exactly the 300s boundary, after the
    // rebirth cooldown has elapsed, must emit a warning.
    t.note_rebirth();
    for _ in 0..MIN_TURNS_AFTER_REBIRTH {
        t.record_prefill(Duration::from_secs(1)); // fast recovery turns
    }
    // First 300s call: consecutive_slow = 1, so no emit yet.
    assert!(
        !t.record_prefill(Duration::from_secs(300)),
        "300s must count as slow but needs 2 consecutive to emit"
    );
    // Second consecutive 300s call: emits.
    assert!(
        t.record_prefill(Duration::from_secs(300)),
        "2nd consecutive 300s prefill must emit"
    );
}

#[test]
fn test_slow_prefill_requires_two_consecutive_slow() {
    // caesar `record_backend_duration`: a single slow prefill must NOT emit;
    // only the 2nd consecutive slow prefill on the same model does.
    let mut t = SlowPrefillTracker::new();
    t.note_rebirth();
    for _ in 0..MIN_TURNS_AFTER_REBIRTH {
        t.record_prefill(Duration::from_secs(1));
    }
    assert!(
        !t.record_prefill(Duration::from_secs(400)),
        "first slow prefill must not emit (needs 2 consecutive)"
    );
    assert!(
        t.record_prefill(Duration::from_secs(400)),
        "second consecutive slow prefill must emit"
    );
}

#[test]
fn test_slow_prefill_cooldown_boundary_suppresses_turns_1_to_4() {
    // REQ-CORE-005 / caesar `record_backend_duration`: within the first
    // `MIN_TURNS_AFTER_REBIRTH` (5) turns after a rebirth, slow-prefill
    // warnings are suppressed. Turns 1..4 must be suppressed; turn 5+ emits.
    let mut t = SlowPrefillTracker::new();
    t.note_rebirth(); // turns_since_rebirth = 0

    // Turns 1..4: each `record_prefill` advances the recovery-turn counter,
    // so turns 1-4 stay below the cooldown and are suppressed.
    for turn in 1..MIN_TURNS_AFTER_REBIRTH {
        assert!(
            !t.record_prefill(Duration::from_secs(400)),
            "turn {turn} (within cooldown) must suppress slow-prefill warnings"
        );
    }

    // Turn 5 (== MIN_TURNS_AFTER_REBIRTH): cooldown elapsed, warning emits.
    assert!(
        t.record_prefill(Duration::from_secs(400)),
        "turn 5 (cooldown elapsed) must emit a slow-prefill warning"
    );
}

#[test]
fn test_slow_prefill_fast_prefill_resets_consecutive_count() {
    // caesar `record_backend_duration`: a fast prefill (< 300s) resets the
    // consecutive-slow counter, so a later slow prefill starts over at 1.
    let mut t = SlowPrefillTracker::new();
    t.note_rebirth();
    for _ in 0..MIN_TURNS_AFTER_REBIRTH {
        t.record_prefill(Duration::from_secs(1));
    }
    assert!(!t.record_prefill(Duration::from_secs(400))); // slow #1
    assert!(!t.record_prefill(Duration::from_secs(1))); // fast resets
    assert!(
        !t.record_prefill(Duration::from_secs(400)),
        "after a reset, a single slow prefill must not emit"
    );
    assert!(
        t.record_prefill(Duration::from_secs(400)),
        "second consecutive slow after reset must emit"
    );
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

    // Fill until token count is between 80% (240) and 90% (270).
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

    assert!(engine.token_count() > advisory_thresh);
    if engine.token_count() <= compact_thresh {
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
            REBIRTH_ADVISORY_MESSAGE
        );
    }

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

    assert!(engine.token_count() > advisory_thresh);
    assert!(engine.should_advise_rebirth());
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
