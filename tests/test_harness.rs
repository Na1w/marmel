//! Phase C integration tests (harness dispatch, FS, and replace).

use marmennill::agents::Agent;
use marmennill::harness::{ToolCaller, ToolInvocation, dispatch_for};
use serde_json::json;

#[test]
fn test_integration_harness_fs_and_dispatch() {
    let tmp_dir = tempfile::tempdir().unwrap();
    let file_path = tmp_dir.path().join("test_integration.txt");
    let path_str = file_path.to_str().unwrap();

    // 1. Write file via harness
    let write_inv = ToolInvocation {
        name: "write_file".to_string(),
        arguments: json!({
            "path": path_str,
            "content": "Line 1: Hello World\nLine 2: Integration Test\nLine 3: Goodbye\n"
        }),
    };
    let res = dispatch_for(&write_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(!res.is_error);

    // 2. Read file with pagination
    let read_inv = ToolInvocation {
        name: "read_file".to_string(),
        arguments: json!({
            "path": path_str,
            "offset": 0,
            "limit": 100
        }),
    };
    let read_res = dispatch_for(&read_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(read_res.content.contains("Hello World"));

    // 3. Replace in file
    let replace_inv = ToolInvocation {
        name: "replace".to_string(),
        arguments: json!({
            "path": path_str,
            "old_str": "Line 2: Integration Test\n",
            "new_str": "Line 2: Replaced Content\n"
        }),
    };
    let replace_res = dispatch_for(&replace_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(!replace_res.is_error);

    // 4. Verify replace took effect
    let verify_res = dispatch_for(&read_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    assert!(verify_res.content.contains("Line 2: Replaced Content"));
}
