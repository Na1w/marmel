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

#[tokio::test(flavor = "multi_thread")]
async fn test_sleep_execution_and_cancellation() {
    // 1. Normal sleep execution with reason
    let sleep_inv = ToolInvocation {
        name: "sleep".to_string(),
        arguments: json!({
            "seconds": 1,
            "reason": "waiting for build"
        }),
    };
    let start = std::time::Instant::now();
    let res = dispatch_for(&sleep_inv, ToolCaller::Specialist(Agent::Coder)).unwrap();
    let elapsed = start.elapsed();
    assert!(!res.is_error);
    assert!(res.content.contains("Slept for 1 seconds"));
    assert!(res.content.contains("waiting for build"));
    assert!(elapsed >= std::time::Duration::from_millis(900));

    // 2. Sleep parameter aliases (duration / duration_seconds)
    let alias_inv = ToolInvocation {
        name: "sleep".to_string(),
        arguments: json!({
            "duration": 1
        }),
    };
    let alias_res = dispatch_for(&alias_inv, ToolCaller::Manager).unwrap();
    assert!(!alias_res.is_error);
    assert!(alias_res.content.contains("Slept for 1 seconds"));

    // 3. Sleep cancelled mid-flight via cancellation token
    marmennill::orchestrator::reset_cancellation();
    let cancel_inv = ToolInvocation {
        name: "sleep".to_string(),
        arguments: json!({
            "seconds": 10
        }),
    };

    // Trigger cancellation after 100ms on a dedicated thread
    std::thread::spawn(|| {
        std::thread::sleep(std::time::Duration::from_millis(100));
        marmennill::orchestrator::cancel_all();
    });

    let cancel_res = dispatch_for(&cancel_inv, ToolCaller::Specialist(Agent::Debugger)).unwrap();
    assert!(cancel_res.is_error);
    assert!(cancel_res.content.contains("interrupted by cancellation"));

    // Reset cancellation token after test
    marmennill::orchestrator::reset_cancellation();
}
