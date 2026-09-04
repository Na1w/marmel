//! Phase B integration tests (monitor / XML rescue fixtures).

use marmennill::harness::monitor::{HarnessMonitor, Intervention};
use serde_json::json;

#[test]
fn test_integration_monitor_xml_rescue_and_repetition() {
    let mut monitor = HarnessMonitor::with_new_stats();

    // 1. Test XML rescue from raw text
    let raw_text = r#"Let me search the files: <tool_call>{"name": "grep_search", "arguments": {"pattern": "fn main"}}</tool_call>"#;
    let rescued = monitor.rescue_xml(raw_text);
    assert_eq!(rescued.len(), 1);
    assert_eq!(rescued[0].function.name, "grep_search");

    // 2. Test repetition detection
    let args = json!({"path": "src/main.rs"});
    let mut blocked = false;
    for _ in 0..10 {
        let intervention = monitor.observe_tool("read_file", &args);
        if matches!(intervention, Intervention::Block | Intervention::Cut) {
            blocked = true;
            break;
        }
    }
    assert!(
        blocked,
        "Repetition monitor must intervene on repeated identical calls"
    );
}

#[test]
fn test_integration_monitor_thought_repetition_across_turns() {
    let mut rep_detector = marmennill::harness::monitor::RepetitionDetector::new(3, 5);

    let thought_turn_1 = "I need to create a basic raytracer in C that renders a scene with a sphere and outputs a PPM image. First, I'll structure the code with core components: a Vector3 type for 3D math, a Ray struct, a Sphere struct with center and radius.\n";
    let response_turn_1 = "I'll help you create a basic raytracer in C. Let's start by writing the complete source code.\n";

    // Turn 1
    rep_detector.push(thought_turn_1);
    rep_detector.push(response_turn_1);
    assert!(
        !rep_detector.is_repeating(),
        "Turn 1 alone should not trigger repetition"
    );

    // Turn 2 (repeating identical thoughts with slightly varied response)
    let thought_turn_2 = "I need to create a basic raytracer in C that renders a scene with a sphere and outputs a PPM image. First, I'll structure the code with core components: a Vector3 type for 3D math, a Ray struct, a Sphere struct with center and radius.\n";
    let response_turn_2 = "I'll assist you create a basic raytracer in C. Let's commence by writing the complete source code.\n";
    rep_detector.push(thought_turn_2);
    rep_detector.push(response_turn_2);

    // Turn 3 (repeating thoughts third time)
    let thought_turn_3 = "I need to create a basic raytracer in C that renders a scene with a sphere and outputs a PPM image. First, I'll structure the code with core components: a Vector3 type for 3D math, a Ray struct, a Sphere struct with center and radius.\n";
    rep_detector.push(thought_turn_3);

    assert!(
        rep_detector.is_repeating(),
        "Repeated thought blocks across turns must trigger repetition detector"
    );
}
