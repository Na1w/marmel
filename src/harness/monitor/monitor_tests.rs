use super::*;
use std::sync::Arc;

use crate::harness::monitor::repetition::{
    LINE_WINDOW, TAIL_SCAN_WINDOW, TEXT_BUFFER_CAPACITY, WORD_WINDOW,
};
use crate::tool_names::{
    TOOL_GREP_SEARCH, TOOL_READ_FILE, TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
};

/// Helper: build a ToolCallRecord for the given name and JSON args.
fn rec(name: &str, args: &str) -> ToolCallRecord {
    let arguments = serde_json::from_str(args).unwrap();
    ToolCallRecord::new(name, arguments)
}

#[test]
fn test_monitor_xml_tool_rescue() {
    // Pattern 1: JSON embedded in <tool_call> tags.
    let json_style = r#"prefix <tool_call>{"function": "read_file", "arguments": {"path": "src/main.rs", "offset": 10}}</tool_call> suffix"#;
    let rescue = XMLToolRescue::new();
    let calls = rescue.rescue(json_style);
    assert_eq!(calls.len(), 1, "JSON-embedded tool call must be rescued");
    assert_eq!(calls[0].function.name, TOOL_READ_FILE);
    assert!(calls[0].id.starts_with("call_text_"));
    // serde_json serializes with sorted keys; compare semantically.
    let parsed: serde_json::Value = serde_json::from_str(&calls[0].function.arguments).unwrap();
    assert_eq!(
        parsed,
        serde_json::json!({"path": "src/main.rs", "offset": 10})
    );

    // Pattern 2: function attribute + JSON body.
    let attr_style =
        r#"<tool_call function="write_file">{"path": "a.txt", "content": "hi"}</tool_call>"#;
    let calls = rescue.rescue(attr_style);
    assert_eq!(calls.len(), 1, "attribute-style tool call must be rescued");
    assert_eq!(calls[0].function.name, TOOL_WRITE_FILE);

    // Legacy SPEC pattern: <function=name> with <parameter> pairs.
    let legacy = r#"tool_call <function=replace><parameter=path>src/main.rs</parameter><parameter=old_str>foo</parameter><parameter=new_str>bar</parameter></function> tool_call"#;
    let calls = rescue.rescue(legacy);
    assert_eq!(calls.len(), 1, "legacy function block must be rescued");
    assert_eq!(calls[0].function.name, TOOL_REPLACE);
    assert!(calls[0].function.arguments.contains("src/main.rs"));

    // Caesar's exact XML pattern: <tool_call><function=name><parameter=key>val</parameter></function></tool_call>.
    let caesar_style = r#"<tool_call><function=read_file><parameter=path>src/main.rs</parameter><parameter=offset>10</parameter></function></tool_call>"#;
    let calls = rescue.rescue(caesar_style);
    assert_eq!(calls.len(), 1, "caesar XML pattern must be rescued");
    assert_eq!(calls[0].function.name, TOOL_READ_FILE);
    assert!(calls[0].function.arguments.contains("src/main.rs"));
    assert!(calls[0].function.arguments.contains("10"));
}

#[test]
fn test_monitor_xml_tool_rescue_increments_stats() {
    let stats = Arc::new(HarnessStats::new());
    let rescue = XMLToolRescue::with_stats(stats.clone());
    let text = r#"<tool_call>{"function": "glob", "arguments": {"pattern": "*.rs"}}</tool_call>"#;
    let calls = rescue.rescue(text);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        stats
            .xml_tool_rescues
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn test_monitor_tool_cycle_detection() {
    // Consecutive identical calls: 3x A blocks (threshold 3).
    let mut det = ToolRepetitionDetector::new(3);
    assert_eq!(det.evaluate(rec("A", r#"{"x": 1}"#)), Intervention::None);
    assert_eq!(det.evaluate(rec("A", r#"{"x": 1}"#)), Intervention::None);
    assert_eq!(det.evaluate(rec("A", r#"{"x": 1}"#)), Intervention::Block);

    // Alternating cycle A→B→A→B→A→B cuts (threshold 3).
    let mut det = ToolRepetitionDetector::new(3);
    let a = rec("A", r#"{"x": 1}"#);
    let b = rec("B", r#"{"x": 2}"#);
    // Sequence: A,B,A,B,A,B (6 calls = 3 full cycles).
    let seq = [&a, &b, &a, &b, &a, &b];
    let mut last = Intervention::None;
    for call in seq {
        last = det.evaluate(call.clone());
    }
    assert_eq!(last, Intervention::Cut, "A→B alternating cycle must be cut");
}

#[test]
fn test_monitor_json_semantic_equality() {
    // Key ordering must be ignored.
    let mut det = ToolRepetitionDetector::new(3);
    assert_eq!(
        det.evaluate(rec(TOOL_READ_FILE, r#"{"a":1,"b":2}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_READ_FILE, r#"{"b":2,"a":1}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_READ_FILE, r#"{"a":1,"b":2}"#)),
        Intervention::Block,
        "swapped argument keys must still be detected as repetition"
    );
}

#[test]
fn test_monitor_pagination_exemption() {
    // Consecutive read_file calls varying only by offset must NOT block.
    let mut det = ToolRepetitionDetector::new(3);
    assert_eq!(
        det.evaluate(rec(TOOL_READ_FILE, r#"{"path":"f","offset":0}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_READ_FILE, r#"{"path":"f","offset":10}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_READ_FILE, r#"{"path":"f","offset":20}"#)),
        Intervention::None,
        "offset-only variation must be exempt from repetition"
    );

    // Consecutive grep_search calls varying only by page must NOT block.
    let mut det = ToolRepetitionDetector::new(3);
    assert_eq!(
        det.evaluate(rec(TOOL_GREP_SEARCH, r#"{"pattern":"x","page":1}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_GREP_SEARCH, r#"{"pattern":"x","page":2}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_GREP_SEARCH, r#"{"pattern":"x","page":3}"#)),
        Intervention::None
    );

    // But identical non-paginated repetition still blocks.
    let mut det = ToolRepetitionDetector::new(3);
    assert_eq!(
        det.evaluate(rec(TOOL_RUN_COMMAND, r#"{"command":"ls"}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_RUN_COMMAND, r#"{"command":"ls"}"#)),
        Intervention::None
    );
    assert_eq!(
        det.evaluate(rec(TOOL_RUN_COMMAND, r#"{"command":"ls"}"#)),
        Intervention::Block
    );
}

#[test]
fn test_monitor_sliding_buffer_capacity() {
    let mut det = ToolRepetitionDetector::new(3);
    for i in 0..100 {
        det.record(rec(
            TOOL_RUN_COMMAND,
            &format!(r#"{{"command":"echo {}"}}"#, i),
        ));
    }
    assert!(det.len() <= 50, "sliding buffer must be capped at 50");
}

#[test]
fn test_monitor_repetition_detector() {
    let mut det = RepetitionDetector::new(5, 5);
    // No repetition yet.
    det.push("hello world this is normal text");
    assert!(!det.is_repeating());

    // Feed a repeated block ≥5 times at the tail.
    let block = "abcde"; // length 5
    for _ in 0..5 {
        det.push(block);
    }
    assert!(
        det.is_repeating(),
        "pattern repeated 5 times at the tail must trigger"
    );

    // Interrupt the pattern: no longer repeating.
    det.push("zzz");
    assert!(!det.is_repeating());
}

#[test]
fn test_monitor_repetition_detector_length_bounds() {
    // Use a non-periodic filler so the buffer itself never forms a longer
    // repeating pattern by coincidence.
    fn non_periodic_fill(det: &mut RepetitionDetector) {
        det.push("The quick brown fox jumps over the lazy dog near the river bank.");
        det.push("Zanzibar's jaguars quickly vex the faithful emu and camel.");
        det.push("Pack my box with five dozen liquor jugs for the party.");
    }

    // A 5-char pattern repeated exactly 4 times must NOT trigger (needs ≥5).
    let mut det = RepetitionDetector::new(5, 5);
    non_periodic_fill(&mut det);
    for _ in 0..4 {
        det.push("abcde");
    }
    assert!(!det.is_repeating(), "4 repeats must not trigger");

    // A short fundamental period is still caught via its length-≥5 period:
    // "ab"×30 forms "ababab" (length 6) repeated 5×, so it correctly fires.
    let mut det = RepetitionDetector::new(5, 5);
    non_periodic_fill(&mut det);
    for _ in 0..30 {
        det.push("ab");
    }
    assert!(
        det.is_repeating(),
        "length-6 period repeated ≥5× must be detected"
    );
}

#[test]
fn test_monitor_line_repetition_detector() {
    let mut det = RepetitionDetector::new(4, 5);
    det.push("Let's go.\nI'll do it.\nWait, I'll check rule 1.\nGood.\n");
    det.push("Let's go.\nI'll do it.\nWait, I'll check rule 2.\nGood.\n");
    det.push("Let's go.\nI'll do it.\nWait, I'll check rule 3.\nGood.\n");
    assert!(!det.is_repeating()); // 3 repeats does not trigger (needs 4)
    det.push("Let's go.\nI'll do it.\nWait, I'll check rule 4.\nGood.\n");
    assert!(det.is_repeating(), "4th repetition of bigrams must trigger");
}

#[test]
fn test_monitor_bigram_no_false_positives_on_diverse_code() {
    let mut det = RepetitionDetector::new(5, 5);
    det.push("#[test]\nfn test_alpha() {\n    assert!(true);\n}\n");
    det.push("#[test]\nfn test_beta() {\n    assert!(true);\n}\n");
    det.push("#[test]\nfn test_gamma() {\n    assert!(true);\n}\n");
    det.push("#[test]\nfn test_delta() {\n    assert!(true);\n}\n");
    assert!(
        !det.is_repeating(),
        "diverse unit test functions must not trigger false positives"
    );
}

#[test]
fn test_monitor_no_false_positives_on_realistic_large_code_file() {
    let mut det = RepetitionDetector::new(5, 5);
    // Write 10 unit tests and functions that share identical boilerplates like return Ok(()),
    // #[tokio::test], assert!(result.is_ok()), and repeated array rows.
    for i in 0..10 {
        det.push(&format!(
            "#[tokio::test]\nasync fn test_case_{i}() {{\n    let arr = [0, 0, 0, 0, 0, 0];\n    let res = calculate({i});\n    assert!(res.is_ok());\n    return Ok(());\n}}\n"
        ));
    }
    assert!(
        !det.is_repeating(),
        "realistic Rust file with 10 unit tests must not trigger false positive repetition"
    );
}

#[test]
fn test_monitor_consecutive_identical_line_spam() {
    let mut det = RepetitionDetector::new(4, 5);
    det.push("Compiling project...\n");
    det.push("thinking about rules\n");
    det.push("thinking about rules\n");
    det.push("thinking about rules\n");
    assert!(!det.is_repeating()); // 3 consecutive repeats does not trigger
    det.push("thinking about rules\n");
    assert!(
        det.is_repeating(),
        "4 consecutive identical lines must trigger repetition break"
    );
}

#[test]
fn test_monitor_word_ngram_phrase_repetition_single_line() {
    let mut det = RepetitionDetector::new(4, 5);
    // Single continuous string without newlines, with varying rule endings
    det.push("I'll do it. Wait, I'll check the rule A. Good. Let's go. ");
    det.push("I'll do it. Wait, I'll check the rule B. Good. Let's go. ");
    det.push("I'll do it. Wait, I'll check the rule C. Good. Let's go. ");
    assert!(!det.is_repeating()); // 3 repeats does not trigger
    det.push("I'll do it. Wait, I'll check the rule D. Good. Let's go. ");
    assert!(
        det.is_repeating(),
        "word 4-gram (e.g. 'wait ill check the') repeating 4 times must trigger"
    );
}

#[test]
fn test_monitor_semantic_json_eq_helper() {
    assert!(semantic_json_eq(r#"{"a":1,"b":2}"#, r#"{"b":2,"a":1}"#));
    // Pagination fields ignored.
    assert!(semantic_json_eq(
        r#"{"path":"f","offset":0}"#,
        r#"{"path":"f","offset":99}"#
    ));
    assert!(semantic_json_eq(
        r#"{"path":"f","page":1}"#,
        r#"{"path":"f","page":2}"#
    ));
    // Different content is not equal.
    assert!(!semantic_json_eq(r#"{"path":"f"}"#, r#"{"path":"g"}"#));
}

// --- Composed HarnessMonitor (runtime facade) ---

#[test]
fn test_monitor_composed_rescue_xml_records_stats() {
    let mon = HarnessMonitor::with_new_stats();
    let text = r#"<tool_call>{"function": "read_file", "arguments": {"path": "a.rs"}}</tool_call>"#;
    let calls = mon.rescue_xml(text);
    assert_eq!(calls.len(), 1);
    assert!(calls[0].id.starts_with("call_text_"));
    assert_eq!(
        mon.stats()
            .xml_tool_rescues
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    // No XML -> no rescue, no stat bump.
    let calls = mon.rescue_xml("plain prose, no tools");
    assert!(calls.is_empty());
    assert_eq!(
        mon.stats()
            .xml_tool_rescues
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
}

#[test]
fn test_monitor_composed_observe_tool_block_and_cut() {
    // Use a threshold-3 config so 3x identical blocks and 3 full cycles cut.
    let cfg = crate::config::MonitoringConfig {
        enabled: true,
        repetition_threshold: 3,
        min_pattern_len: 5,
        ..Default::default()
    };
    let mut mon = HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
    let args = serde_json::json!({"x": 1});
    // 3x identical -> Block.
    assert_eq!(mon.observe_tool("A", &args), Intervention::None);
    assert_eq!(mon.observe_tool("A", &args), Intervention::None);
    assert_eq!(mon.observe_tool("A", &args), Intervention::Block);
    // SPEC error payload is present for Block.
    let err = mon.intervention_error(Intervention::Block).unwrap();
    assert!(err.contains("TOOL REPETITION DETECTED"));

    // Alternating cycle A→B→A→B→A→B -> Cut.
    let mut mon = HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
    let a = serde_json::json!({"x": 1});
    let b = serde_json::json!({"x": 2});
    let mut last = Intervention::None;
    for call in [&a, &b, &a, &b, &a, &b] {
        last = mon.observe_tool(if call == &a { "A" } else { "B" }, call);
    }
    assert_eq!(last, Intervention::Cut);
    let err = mon.intervention_error(Intervention::Cut).unwrap();
    assert!(err.contains("TOOL CYCLE DETECTED"));
    assert!(mon.intervention_error(Intervention::None).is_none());
}

#[test]
fn test_monitor_composed_text_repetition_truncates_and_counts() {
    let mut mon = HarnessMonitor::with_new_stats();
    // Normal text: no break.
    mon.feed_text("This is a perfectly normal sentence for the model to output.");
    assert!(!mon.repetition_fired);

    // Feed a 5-char pattern repeated 5x: break fires exactly once.
    for _ in 0..5 {
        let fired = mon.feed_text("abcde");
        if fired {
            break;
        }
    }
    assert!(mon.repetition_fired);
    assert_eq!(
        mon.stats()
            .repetition_breaks
            .load(std::sync::atomic::Ordering::Relaxed),
        1,
        "one break must increment repetition_breaks once"
    );

    // The repeated block was truncated: a follow-up feed on the reset buffer
    // does not double-count.
    mon.feed_text("still fine now");
    assert_eq!(
        mon.stats()
            .repetition_breaks
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );

    // After re-arming, a fresh independent pattern can break again.
    mon.reset_text_break();
    for _ in 0..5 {
        if mon.feed_text("xyzzy") {
            break;
        }
    }
    assert_eq!(
        mon.stats()
            .repetition_breaks
            .load(std::sync::atomic::Ordering::Relaxed),
        2
    );
}

// --- Config-driven thresholds (t-d403 parity with caesar) ---

#[test]
fn test_monitor_config_driven_tool_threshold() {
    // A threshold-4 config must block only on the 4th identical call.
    let cfg = crate::config::MonitoringConfig {
        enabled: true,
        repetition_threshold: 4,
        min_pattern_len: 5,
        ..Default::default()
    };
    let mut mon = HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
    let args = serde_json::json!({"x": 1});
    assert_eq!(mon.observe_tool("A", &args), Intervention::None);
    assert_eq!(mon.observe_tool("A", &args), Intervention::None);
    assert_eq!(mon.observe_tool("A", &args), Intervention::None);
    assert_eq!(
        mon.observe_tool("A", &args),
        Intervention::Block,
        "4th identical call must block with threshold 4"
    );
    // The SPEC error reflects the configured threshold.
    let err = mon.intervention_error(Intervention::Block).unwrap();
    assert!(
        err.contains("4 times in a row"),
        "error must reflect threshold 4"
    );
}

#[test]
fn test_monitor_config_driven_cycle_threshold() {
    // A threshold-2 config cuts after 2 full A→B cycles (4 calls).
    let cfg = crate::config::MonitoringConfig {
        enabled: true,
        repetition_threshold: 2,
        min_pattern_len: 5,
        ..Default::default()
    };
    let mut mon = HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
    let a = serde_json::json!({"x": 1});
    let b = serde_json::json!({"x": 2});
    let mut last = Intervention::None;
    for call in [&a, &b, &a, &b] {
        last = mon.observe_tool(if call == &a { "A" } else { "B" }, call);
    }
    assert_eq!(
        last,
        Intervention::Cut,
        "2 full A→B cycles must cut with threshold 2"
    );
}

#[test]
fn test_monitor_config_driven_text_min_len() {
    // A min_pattern_len of 7 means a 5-char pattern repeated 5x does NOT
    // trigger, but a 7-char pattern repeated 5x does.
    let cfg = crate::config::MonitoringConfig {
        enabled: true,
        repetition_threshold: 5,
        min_pattern_len: 7,
        ..Default::default()
    };
    let mut mon = HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
    // 5-char pattern repeated 5x: below min_len 7, must NOT fire.
    for _ in 0..5 {
        mon.feed_text("abcde");
    }
    assert!(
        !mon.repetition_fired,
        "5-char pattern must not fire with min_len 7"
    );

    // 7-char pattern repeated 5x: meets min_len, must fire.
    let mut mon = HarnessMonitor::new_with_config(std::sync::Arc::new(HarnessStats::new()), &cfg);
    for _ in 0..5 {
        if mon.feed_text("abcdefg") {
            break;
        }
    }
    assert!(mon.repetition_fired, "7-char pattern repeated 5x must fire");
}

#[test]
fn test_monitor_config_defaults_match_caesar() {
    // caesar defaults: repetition_threshold = 5, min_pattern_len = 5.
    let cfg = crate::config::MonitoringConfig::default();
    assert_eq!(cfg.repetition_threshold, 5);
    assert_eq!(cfg.min_pattern_len, 5);
    assert!(cfg.enabled);
}

// --- Orphan-message pruning (t-d403 parity with caesar) ---

#[test]
fn test_monitor_prune_orphan_tool_messages() {
    let assistant = Message::Assistant {
        content: Some("calling".to_string()),
        reasoning_content: None,
        tool_calls: vec![ToolCall::new(
            "call_1",
            TOOL_READ_FILE,
            r#"{"path":"a.rs"}"#,
        )],
    };
    let orphan = Message::Tool {
        tool_call_id: "call_orphan".to_string(),
        content: "no matching assistant".to_string(),
    };
    let matched = Message::Tool {
        tool_call_id: "call_1".to_string(),
        content: "fn main() {}".to_string(),
    };
    let user = Message::User {
        content: "hi".to_string(),
    };

    let pruned = prune_orphan_tool_messages(vec![user, assistant, orphan, matched]);
    // The orphan tool message is dropped; the matched one and others survive.
    assert_eq!(pruned.len(), 3);
    // Exactly one tool message remains, and it is the matched one.
    let tool_msgs: Vec<&Message> = pruned
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .collect();
    assert_eq!(tool_msgs.len(), 1, "only the matched tool message survives");
    match tool_msgs[0] {
        Message::Tool { tool_call_id, .. } => {
            assert_eq!(tool_call_id, "call_1");
        }
        _ => unreachable!(),
    }
    // The user message survives.
    assert!(pruned.iter().any(|m| matches!(m, Message::User { .. })));
}

#[test]
fn test_monitor_prune_orphan_keeps_all_when_all_matched() {
    let assistant = Message::Assistant {
        content: Some("calling".to_string()),
        reasoning_content: None,
        tool_calls: vec![
            ToolCall::new("call_1", TOOL_READ_FILE, r#"{"path":"a.rs"}"#),
            ToolCall::new("call_2", TOOL_GREP_SEARCH, r#"{"pattern":"x"}"#),
        ],
    };
    let t1 = Message::Tool {
        tool_call_id: "call_1".to_string(),
        content: "a".to_string(),
    };
    let t2 = Message::Tool {
        tool_call_id: "call_2".to_string(),
        content: "b".to_string(),
    };
    let pruned = prune_orphan_tool_messages(vec![assistant, t1, t2]);
    assert_eq!(pruned.len(), 3, "all tool messages matched, none pruned");
    let tool_msgs: Vec<&Message> = pruned
        .iter()
        .filter(|m| matches!(m, Message::Tool { .. }))
        .collect();
    assert_eq!(tool_msgs.len(), 2, "both tool messages survive");
}

#[test]
fn test_monitor_consecutive_rebirth_blocked() {
    let mut monitor = HarnessMonitor::new(std::sync::Arc::new(HarnessStats::new()));
    let args1 = serde_json::json!({ "summary": "first checkpoint" });
    let args2 = serde_json::json!({ "summary": "second checkpoint with different text" });

    // First rebirth is permitted
    assert_eq!(
        monitor.observe_tool(crate::tool_names::TOOL_REBIRTH, &args1),
        Intervention::None
    );

    // Consecutive rebirth is immediately blocked even with different summary
    assert_eq!(
        monitor.observe_tool(crate::tool_names::TOOL_REBIRTH, &args2),
        Intervention::Block
    );
    let err = monitor.intervention_error(Intervention::Block).unwrap();
    assert!(err.contains("Rebirth checkpoint was already applied"));

    // Running another tool allows rebirth again
    let read_args = serde_json::json!({ "path": "src/lib.rs" });
    assert_eq!(
        monitor.observe_tool(crate::tool_names::TOOL_READ_FILE, &read_args),
        Intervention::None
    );

    assert_eq!(
        monitor.observe_tool(crate::tool_names::TOOL_REBIRTH, &args2),
        Intervention::None
    );
}

// --- Bounded scan windows (recon M3 regression guard) ---

/// Non-repeating filler: every line is distinct (the numbers differ), so the
/// filler itself never trips any of the repetition rules.
fn filler(n: usize) -> String {
    format!("Reading file {n} and checking line {n} for marker {n} now.\n")
}

/// Push filler text until `det` holds at least `target` characters.
fn fill_until(det: &mut RepetitionDetector, target: usize) {
    let mut n = 0usize;
    while det.len() < target {
        det.push(&filler(n));
        n += 1;
    }
}

#[test]
fn test_monitor_repetition_window_bounded_after_long_streams() {
    let mut det = RepetitionDetector::new(3, 5);
    // Stream far more text than any window (and more than the rolling buffer
    // capacity): the rolling state must stay pinned at its caps.
    for n in 0..400 {
        det.push(&filler(n));
    }
    assert_eq!(det.len(), TEXT_BUFFER_CAPACITY, "buffer cap");
    assert_eq!(
        det.tail_scan_chars(),
        TAIL_SCAN_WINDOW,
        "the tail scan must look at a fixed window, not the whole buffer"
    );
    assert!(
        det.tail_scan_candidates() <= TAIL_SCAN_WINDOW / 3,
        "candidate pattern lengths must be capped by the scan window"
    );
    assert!(
        det.line_window_len() <= LINE_WINDOW,
        "the line window must be bounded, got {}",
        det.line_window_len()
    );
    assert!(
        det.word_window_len() <= WORD_WINDOW,
        "the word window must be bounded, got {}",
        det.word_window_len()
    );

    // A genuine repetition is still detected once the buffer is saturated.
    for _ in 0..5 {
        det.push("abcdefghij");
    }
    assert!(
        det.is_repeating(),
        "a 10-char pattern repeated 5x must fire on a saturated buffer"
    );
}

#[test]
fn test_monitor_repetition_scan_work_is_bounded() {
    // The per-delta scan budget must be a property of the windows, not of how
    // much text has streamed by. These counts are the *before* regression: the
    // old scan grew linearly (and its cost quadratically) with the buffer.
    let small = {
        let mut det = RepetitionDetector::new(5, 5);
        fill_until(&mut det, 300);
        det
    };
    let huge = {
        let mut det = RepetitionDetector::new(5, 5);
        for n in 0..400 {
            det.push(&filler(n));
        }
        det
    };
    assert!(small.len() >= 300 && small.len() < 400);
    assert_eq!(huge.len(), TEXT_BUFFER_CAPACITY);

    assert!(
        huge.tail_scan_chars() <= TAIL_SCAN_WINDOW,
        "tail scan window must be bounded by TAIL_SCAN_WINDOW, got {}",
        huge.tail_scan_chars()
    );
    assert!(
        huge.tail_scan_candidates() <= TAIL_SCAN_WINDOW / 5,
        "tail scan candidates must be bounded, got {}",
        huge.tail_scan_candidates()
    );
    assert!(
        huge.line_window_len() <= LINE_WINDOW && huge.word_window_len() <= WORD_WINDOW,
        "rolling windows must be bounded (lines={} words={})",
        huge.line_window_len(),
        huge.word_window_len()
    );

    // Benchmark-style guard on the *measured* cost. Before the fix, one
    // `is_repeating` call on a saturated 16 384-char buffer measured ~256 ms in
    // this debug build (23 s for a 4 500-delta stream in the recon replica).
    // The bounded scan has to stay three orders of magnitude below that.
    let mut det = huge;
    let started = std::time::Instant::now();
    for n in 0..300usize {
        det.push(&filler(n));
        let _ = det.is_repeating();
    }
    let elapsed = started.elapsed();
    println!("300 saturated push+is_repeating cycles: {elapsed:?}");
    // The same 300 deltas measured ~77 s before the fix (256 ms per scan on a
    // saturated buffer in this debug build). The bounded scan must stay far
    // below that; 4 s leaves a wide margin over the measured ~0.7 s.
    assert!(
        elapsed < std::time::Duration::from_secs(4),
        "the per-delta repetition scan is no longer bounded: 300 saturated \
         cycles took {elapsed:?}"
    );
}

// --- Rescued argument keys & fenced-code awareness (recon M5 regression guard) ---

/// Search-tool call written in the attribute/body encodings.
fn grep_call_body(pattern: &str) -> String {
    format!(r#"<tool_call function="{TOOL_GREP_SEARCH}">{pattern}</tool_call>"#)
}

/// Parse the arguments of a rescued call back into JSON.
fn rescued_args(call: &ToolCall) -> serde_json::Value {
    serde_json::from_str(&call.function.arguments).expect("rescued arguments must be a JSON object")
}

#[test]
fn test_monitor_xml_rescue_search_call_carries_pattern_key() {
    let rescue = XMLToolRescue::new();

    // Body encoding: the schema key of the search tool is `pattern`
    // (`crate::harness::search::grep_search`), never `query`.
    let calls = rescue.rescue(&grep_call_body("fn main"));
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].function.name, TOOL_GREP_SEARCH);
    assert_eq!(
        rescued_args(&calls[0]),
        serde_json::json!({"pattern": "fn main"})
    );
    assert!(
        !calls[0].function.arguments.contains("query"),
        "the rescuer must not emit the non-schema key `query`, got {}",
        calls[0].function.arguments
    );

    // `query=` remains an accepted INPUT alias on the opening tag, but the
    // rescued payload is normalised to `pattern`.
    let alias = format!(r#"<tool_call function="{TOOL_GREP_SEARCH}" query="needle"></tool_call>"#);
    let calls = rescue.rescue(&alias);
    assert_eq!(calls.len(), 1, "`query=` must still be accepted as input");
    assert_eq!(
        rescued_args(&calls[0]),
        serde_json::json!({"pattern": "needle"})
    );

    // Both spellings present: the canonical key wins.
    let both = format!(
        r#"<tool_call function="{TOOL_GREP_SEARCH}" pattern="canon" query="alias"></tool_call>"#
    );
    let calls = rescue.rescue(&both);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        rescued_args(&calls[0]),
        serde_json::json!({"pattern": "canon"})
    );

    // JSON-embedded encoding: alias renamed on the way out as well.
    let embedded = format!(
        r#"<tool_call>{{"function": "{TOOL_GREP_SEARCH}", "arguments": {{"query": "todo"}}}}</tool_call>"#
    );
    let calls = rescue.rescue(&embedded);
    assert_eq!(calls.len(), 1);
    assert_eq!(
        rescued_args(&calls[0]),
        serde_json::json!({"pattern": "todo"})
    );
}

#[test]
fn test_monitor_xml_rescue_skips_fenced_code_examples() {
    let rescue = XMLToolRescue::new();
    let call = format!(r#"<tool_call function="{TOOL_READ_FILE}" path="a.rs"></tool_call>"#);

    // A tool-call example shown inside a ``` fence is documentation, not a call.
    let fenced = format!("Here is the syntax:\n```xml\n{call}\n```\nEnd of example.");
    assert!(
        rescue.rescue(&fenced).is_empty(),
        "an XML tool call inside a fenced code block must not be rescued"
    );

    // ~~~ fences are fences too.
    let tilde = format!("~~~markdown\n{call}\n~~~\n");
    assert!(
        rescue.rescue(&tilde).is_empty(),
        "a ~~~ fenced example must not be rescued"
    );

    // A closing fence must be at least as long as the opener (plan_parse rule),
    // so the inner ``` does not end the ```` block.
    let longer = format!("````xml\n```\n{call}\n```\n````\n");
    assert!(
        rescue.rescue(&longer).is_empty(),
        "a shorter inner fence must not close the outer fence"
    );

    // An unterminated fence swallows the remainder of the text.
    let open = format!("prose\n```\n{call}");
    assert!(
        rescue.rescue(&open).is_empty(),
        "an unterminated fence must suppress the rest of the text"
    );

    // The very same XML outside fences is still rescued.
    let outside = format!("prose {call} tail");
    assert_eq!(
        rescue.rescue(&outside).len(),
        1,
        "the same call outside fences must be rescued"
    );

    // Mixed: only the unfenced occurrence is rescued.
    let mixed = format!("```xml\n{call}\n```\nand the real one: {call}");
    let calls = rescue.rescue(&mixed);
    assert_eq!(calls.len(), 1, "exactly the unfenced call must be rescued");
    assert_eq!(calls[0].function.name, TOOL_READ_FILE);

    // Inline single backticks are not fences.
    let inline = format!("use `{call}` inline");
    assert_eq!(
        rescue.rescue(&inline).len(),
        1,
        "single backticks are not a code fence"
    );

    // The legacy encoding is fence-aware too.
    let p_close = "</para".to_string() + "meter>";
    let legacy = format!(
        "tool_call <function={TOOL_READ_FILE}><parameter=path>a.rs{p_close}</function> tool_call"
    );
    let fenced_legacy = format!("```\n{legacy}\n```\n");
    assert!(
        rescue.rescue(&fenced_legacy).is_empty(),
        "a legacy tool-call example inside a fence must not be rescued"
    );
    assert_eq!(
        rescue.rescue(&legacy).len(),
        1,
        "the same legacy call outside a fence must be rescued"
    );

    // Guard the other direction: a *real* call whose body happens to contain a
    // fence is still rescued, because the fence sits inside the call instead of
    // around it.
    let close = "</".to_string() + "tool_call>";
    let body_with_fence = format!(
        r#"<tool_call function="{TOOL_WRITE_FILE}" path="notes.md">```rust
fn main() {{}}
```
{close}"#
    );
    let calls = rescue.rescue(&body_with_fence);
    assert_eq!(
        calls.len(),
        1,
        "a fence inside the call body must not suppress the call itself"
    );
    assert_eq!(calls[0].function.name, TOOL_WRITE_FILE);
}

#[test]
fn test_monitor_xml_rescue_fenced_only_does_not_bump_stats() {
    let stats = Arc::new(HarnessStats::new());
    let rescue = XMLToolRescue::with_stats(stats.clone());
    let call = format!(r#"<tool_call function="{TOOL_READ_FILE}" path="a.rs"></tool_call>"#);
    let fenced = format!("```xml\n{call}\n```\n");
    assert!(rescue.rescue(&fenced).is_empty());
    assert_eq!(
        stats
            .xml_tool_rescues
            .load(std::sync::atomic::Ordering::Relaxed),
        0,
        "a skipped fenced example must not count as an intervention"
    );
}
