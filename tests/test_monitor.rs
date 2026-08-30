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
