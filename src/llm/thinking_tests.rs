use super::*;
use crate::types::Message;

fn msg(content: &str) -> Message {
    Message::User {
        content: content.to_string(),
    }
}

fn req() -> ChatRequest {
    ChatRequest {
        model: "m".to_string(),
        messages: vec![msg("hi")],
        temperature: Some(0.7),
        top_p: None,
        frequency_penalty: Some(0.2),
        presence_penalty: None,
        stream: Some(true),
        enable_thinking: Some(true),
        tools: None,
    }
}

/// REQ-LLM-002: interleaved `[thinking]` tags separate the reasoning
/// channel from the main payload, even when tags span delta boundaries.
#[test]
fn test_llm_stream_thinking_demux() {
    let raw = "Hello[thinking]Let me parse this carefully[/thinking] world";
    let mut d = ThinkingDemuxer::new();
    // Feed it one char at a time to exercise incremental tag-boundary handling.
    for ch in raw.chars() {
        d.push(&ch.to_string());
    }
    assert_eq!(
        d.content(),
        "Hello world",
        "tags must be stripped from payload"
    );
    assert_eq!(
        d.thinking(),
        "Let me parse this carefully",
        "thinking content routed to dedicated channel"
    );
    assert!(
        !d.is_in_thinking(),
        "must not be left inside a thinking block"
    );
}

/// REQ-LLM-002: `preserve_thinking = true` keeps the raw tags in the payload.
#[test]
fn test_llm_thinking_preserve() {
    let mut d = ThinkingDemuxer::with_preserve(true);
    d.demux_all("a[thinking]b[/thinking]c");
    assert_eq!(
        d.content(),
        "a[thinking]b[/thinking]c",
        "tags preserved in payload when configured"
    );
    assert_eq!(d.thinking(), "b");
}

/// REQ-LLM-002: into_message carries reasoning in a separate field.
#[test]
fn test_llm_into_message_strips_thinking() {
    let mut d = ThinkingDemuxer::new();
    d.demux_all("vis[thinking]reason[/thinking]ible");
    let m = d.into_message();
    match m {
        Message::Assistant {
            content,
            reasoning_content,
            tool_calls,
        } => {
            assert_eq!(content.as_deref(), Some("visible"));
            assert_eq!(reasoning_content.as_deref(), Some("reason"));
            assert!(tool_calls.is_empty());
        }
        _ => panic!("expected assistant message"),
    }
}

/// REQ-LLM-003: a recovery request disables thinking and shifts
/// frequency_penalty (+0.5) and temperature (+0.1) for one turn.
#[test]
fn test_llm_recovery_suppresses_thinking() {
    let base = req();
    let recovered = apply_recovery(&base, RecoveryAdjustment::default());

    assert_eq!(recovered.enable_thinking, Some(false));
    assert_eq!(
        recovered.frequency_penalty,
        Some(0.2 + 0.5),
        "frequency_penalty += 0.5"
    );
    assert_eq!(recovered.temperature, Some(0.7 + 0.1), "temperature += 0.1");

    // Original request must be untouched (one-turn semantics).
    assert_eq!(base.enable_thinking, Some(true));
    assert_eq!(base.frequency_penalty, Some(0.2));
    assert_eq!(base.temperature, Some(0.7));
}

/// REQ-LLM-003: recovery works when penalties/temperature are None.
#[test]
fn test_llm_recovery_defaults_when_absent() {
    let mut base = req();
    base.frequency_penalty = None;
    base.temperature = None;
    let recovered = apply_recovery(&base, RecoveryAdjustment::default());
    assert_eq!(recovered.frequency_penalty, Some(0.5));
    assert_eq!(recovered.temperature, Some(0.8)); // 0.7 default + 0.1
}

// ---------------------------------------------------------------------------
// t-035d — recovery parameters must be clamped into the documented ranges.
// Before the fix `frequency_penalty = 2.0` became `2.5` and `temperature = 2.0`
// became `2.1`, both of which the provider rejects with HTTP 400, so the
// recovery turn could never recover.
// ---------------------------------------------------------------------------

fn approx_eq(a: f32, b: f32) -> bool {
    (a - b).abs() < 1e-3
}

fn clamp_of<'a>(outcome: &'a RecoveryOutcome, param: &str) -> Option<&'a RecoveryClamp> {
    outcome.clamps.iter().find(|c| c.param == param)
}

/// Every documented range is enforced on the request that is actually issued,
/// and each rewrite is reported as a typed [`RecoveryClamp`].
#[test]
fn test_llm_recovery_clamps_above_range_parameters() {
    let mut base = req();
    base.temperature = Some(2.0); // +0.1 -> 2.1 -> 2.0
    base.frequency_penalty = Some(2.0); // +0.5 -> 2.5 -> 2.0
    base.presence_penalty = Some(3.5); //  -> 2.0
    base.top_p = Some(1.5); //  -> 1.0

    let outcome = apply_recovery_report(&base, RecoveryAdjustment::default());

    for clamp in &outcome.clamps {
        let (min, max) = clamp.valid_range;
        assert!(
            (min..=max).contains(&clamp.applied),
            "{} clamped outside its documented range: {} -> {}",
            clamp.param,
            clamp.requested,
            clamp.applied
        );
        assert_ne!(
            clamp.requested, clamp.applied,
            "a recorded clamp must actually have moved the value"
        );
    }

    let temperature = clamp_of(&outcome, "temperature").expect("temperature clamp recorded");
    assert_eq!(temperature.reason, ClampReason::AboveRange);
    assert!(
        approx_eq(temperature.requested, 2.1),
        "requested was {}",
        temperature.requested
    );
    assert_eq!(temperature.applied, MAX_TEMPERATURE);
    assert_eq!(temperature.valid_range, (MIN_TEMPERATURE, MAX_TEMPERATURE));

    let frequency = clamp_of(&outcome, "frequency_penalty").expect("frequency clamp recorded");
    assert_eq!(frequency.requested, 2.5);
    assert_eq!(frequency.applied, MAX_PENALTY);
    assert_eq!(frequency.reason, ClampReason::AboveRange);

    assert_eq!(
        clamp_of(&outcome, "presence_penalty").map(|c| (c.requested, c.applied)),
        Some((3.5, MAX_PENALTY))
    );
    assert_eq!(
        clamp_of(&outcome, "top_p").map(|c| (c.requested, c.applied)),
        Some((1.5, MAX_TOP_P))
    );

    // The request that goes on the wire carries the clamped values.
    assert_eq!(outcome.request.temperature, Some(MAX_TEMPERATURE));
    assert_eq!(outcome.request.frequency_penalty, Some(MAX_PENALTY));
    assert_eq!(outcome.request.presence_penalty, Some(MAX_PENALTY));
    assert_eq!(outcome.request.top_p, Some(MAX_TOP_P));
    assert_eq!(outcome.request.enable_thinking, Some(false));

    // The source request is untouched (one-turn semantics preserved).
    assert_eq!(base.temperature, Some(2.0));
    assert_eq!(base.frequency_penalty, Some(2.0));
    assert_eq!(base.presence_penalty, Some(3.5));

    // And the serialized body is well-formed: every sampling number in it is
    // inside its documented range.
    let body = serde_json::to_value(&outcome.request).expect("request body serializes");
    for (field, (min, max)) in [
        ("temperature", (MIN_TEMPERATURE, MAX_TEMPERATURE)),
        ("top_p", (MIN_TOP_P, MAX_TOP_P)),
        ("frequency_penalty", (MIN_PENALTY, MAX_PENALTY)),
        ("presence_penalty", (MIN_PENALTY, MAX_PENALTY)),
    ] {
        if let Some(value) = body.get(field).and_then(|v| v.as_f64()) {
            assert!(
                (f64::from(min)..=f64::from(max)).contains(&value),
                "`{field}` left the documented range in the serialized body: {value}"
            );
        }
    }
}

/// Below-range shifts (a caller-supplied negative delta) and non-finite values are
/// clamped too — never serialized as `NaN`/`inf`, never sent as-is.
#[test]
fn test_llm_recovery_clamps_below_range_and_non_finite_parameters() {
    let mut base = req();
    base.temperature = Some(0.2);
    base.frequency_penalty = Some(0.0);
    base.presence_penalty = Some(f32::INFINITY);
    let adj = RecoveryAdjustment {
        frequency_penalty_delta: -2.5,
        temperature_delta: -0.5,
    };
    let outcome = apply_recovery_report(&base, adj);

    let temperature = clamp_of(&outcome, "temperature").expect("temperature clamp recorded");
    assert!(approx_eq(temperature.requested, -0.3));
    assert_eq!(temperature.applied, MIN_TEMPERATURE);
    assert_eq!(temperature.reason, ClampReason::BelowRange);

    let frequency = clamp_of(&outcome, "frequency_penalty").expect("frequency clamp recorded");
    assert_eq!(frequency.requested, -2.5);
    assert_eq!(frequency.applied, MIN_PENALTY);
    assert_eq!(frequency.reason, ClampReason::BelowRange);

    let presence = clamp_of(&outcome, "presence_penalty").expect("presence clamp recorded");
    assert_eq!(presence.reason, ClampReason::NotFinite);
    assert_eq!(presence.applied, MIN_PENALTY);

    let mut nan = req();
    nan.temperature = Some(f32::NAN);
    let outcome = apply_recovery_report(&nan, RecoveryAdjustment::default());
    let temperature = clamp_of(&outcome, "temperature").expect("NaN temperature clamped");
    assert_eq!(temperature.reason, ClampReason::NotFinite);
    assert_eq!(temperature.applied, MIN_TEMPERATURE);
    assert_eq!(outcome.request.temperature, Some(MIN_TEMPERATURE));
    // A NaN would serialize as `null` and is rejected; the clamped body is a number.
    let body = serde_json::to_value(&outcome.request).expect("serializes");
    assert!(body.get("temperature").and_then(|v| v.as_f64()).is_some());
}

/// An already well-formed request is passed through unmodified apart from the
/// recovery shift itself, and reports no clamps.
#[test]
fn test_llm_recovery_in_range_parameters_are_not_clamped() {
    let base = req();
    let outcome = apply_recovery_report(&base, RecoveryAdjustment::default());
    assert!(
        outcome.clamps.iter().all(|c| c.param != "top_p"),
        "unset/valid pass-through parameters must not be rewritten: {:?}",
        outcome.clamps
    );
    assert!(approx_eq(outcome.request.temperature.unwrap(), 0.8));
    assert!(approx_eq(outcome.request.frequency_penalty.unwrap(), 0.7));
    assert_eq!(outcome.request.top_p, None);
    assert_eq!(outcome.request.presence_penalty, None);
}

/// The clamp is not silent: `apply_recovery` logs it once per request, old → new.
#[test]
fn test_llm_recovery_clamp_is_logged_once_with_old_and_new_value() {
    use std::io::Write;

    thread_local! {
        static LINES: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
    }

    struct Sink;
    impl Write for Sink {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            LINES.with(|l| {
                l.borrow_mut()
                    .push(String::from_utf8_lossy(buf).into_owned())
            });
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    struct Maker;
    impl<'w> tracing_subscriber::fmt::MakeWriter<'w> for Maker {
        type Writer = Sink;
        fn make_writer(&self) -> Sink {
            Sink
        }
    }

    let subscriber = tracing_subscriber::fmt()
        .with_ansi(false)
        .with_max_level(tracing::Level::WARN)
        .with_writer(Maker)
        .finish();

    let mut base = req();
    base.frequency_penalty = Some(2.0);

    LINES.with(|l| l.borrow_mut().clear());
    let recovered = tracing::subscriber::with_default(subscriber, || {
        apply_recovery(&base, RecoveryAdjustment::default())
    });
    let logged = LINES.with(|l| l.borrow().join("\n"));

    assert_eq!(recovered.frequency_penalty, Some(MAX_PENALTY));
    assert!(
        logged.contains("WARN") && logged.contains("frequency_penalty 2.5 -> 2"),
        "the clamp must be logged with its old -> new value, got: {logged:?}"
    );
    let clamp_lines = logged
        .lines()
        .filter(|line| line.contains("frequency_penalty 2.5 -> 2"))
        .count();
    assert_eq!(
        clamp_lines, 1,
        "exactly one clamp line per request, got {clamp_lines}: {logged:?}"
    );
}

/// REQ-LLM-004: nudge policy allows up to 3 empty-production attempts.
#[test]
fn test_llm_empty_production_nudges() {
    let policy = NudgePolicy::default();
    assert!(policy.should_nudge(0), "attempt 1 of 3 allowed");
    assert!(policy.should_nudge(1), "attempt 2 of 3 allowed");
    assert!(policy.should_nudge(2), "attempt 3 of 3 allowed");
    assert!(!policy.should_nudge(3), "3 used -> terminal error");

    let messages = vec![msg("hi")];
    let nudged = policy.nudge(messages.clone());
    assert_eq!(nudged.len(), 2, "a user nudge is appended");
    match &nudged[1] {
        Message::User { content } => assert_eq!(content, "?"),
        _ => panic!("nudge must be a user message"),
    }
}

/// REQ-LLM-002: multi-byte UTF-8 characters and emojis (e.g. 🔍, 🚀)
/// must not panic on char boundary slicing during partial tag detection.
#[test]
fn test_llm_stream_thinking_demux_multibyte_utf8() {
    let raw = "1. **Analyze**: analyze this code. 🔍 [thinking]reasoning steps 🚀[/thinking] done!";
    let mut d = ThinkingDemuxer::new();
    for ch in raw.chars() {
        d.push(&ch.to_string());
    }
    assert_eq!(d.content(), "1. **Analyze**: analyze this code. 🔍  done!");
    assert_eq!(d.thinking(), "reasoning steps 🚀");
}

#[test]
fn test_partial_prefix_split_multibyte() {
    // String ending with multi-byte emoji followed by partial tag prefix.
    let buf = "Hello 🔍[thin";
    assert_eq!(
        partial_prefix_split(buf, "[thinking]"),
        Some("Hello 🔍".len())
    );

    // String ending directly with multi-byte emoji (no tag prefix).
    let buf_emoji = "Hello 🔍";
    assert_eq!(partial_prefix_split(buf_emoji, "[thinking]"), None);
}

#[test]
fn test_llm_stream_think_tags_demux() {
    let raw = "<think>\nI should report this honestly.\n</think>\nVisible message to user";
    let mut d = ThinkingDemuxer::new();
    for ch in raw.chars() {
        d.push(&ch.to_string());
    }
    assert_eq!(d.content(), "\nVisible message to user");
    assert_eq!(d.thinking(), "\nI should report this honestly.\n");
}

#[test]
fn test_llm_stream_thinking_incremental_emission() {
    let mut d = ThinkingDemuxer::new();
    let mut emitted_content = String::new();
    let mut emitted_thinking = String::new();

    let chunks = vec![
        "Hello, ",
        "[thin",
        "king]Let me ",
        "reason[/thin",
        "king] world!",
    ];

    for chunk in chunks {
        d.push_delta(chunk, |kind, text| match kind {
            DeltaKind::Content => emitted_content.push_str(text),
            DeltaKind::Thinking => emitted_thinking.push_str(text),
        });
    }
    d.finish_delta(|kind, text| match kind {
        DeltaKind::Content => emitted_content.push_str(text),
        DeltaKind::Thinking => emitted_thinking.push_str(text),
    });

    assert_eq!(emitted_content, "Hello,  world!");
    assert_eq!(emitted_thinking, "Let me reason");
    assert_eq!(d.content(), "Hello,  world!");
    assert_eq!(d.thinking(), "Let me reason");
}
