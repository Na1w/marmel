//! Integration tests for Specialist & Validator subagent message history,
//! tool schema allowlist filtering, and deliverable validation.

use marmennill::agents::Agent;
use marmennill::orchestrator::SpecialistRegistry;
use marmennill::types::{Message, ToolCall, ToolDef, ToolFunction};

#[test]
fn test_specialist_tool_filtering_and_allowlists() {
    let registry = SpecialistRegistry::canonical();
    let all_tools = ToolDef::default_tools();

    // 1. Researcher must have write_file, replace, read_file, run_command, grep_search, glob, delegate_task
    let researcher_entry = registry
        .resolve(Agent::Researcher)
        .expect("researcher is registered");
    let researcher_tools: Vec<String> = all_tools
        .iter()
        .filter(|t| researcher_entry.allows(&t.function.name))
        .map(|t| t.function.name.clone())
        .collect();

    assert!(researcher_tools.contains(&"write_file".to_string()));
    assert!(researcher_tools.contains(&"replace".to_string()));
    assert!(researcher_tools.contains(&"read_file".to_string()));
    assert!(researcher_tools.contains(&"run_command".to_string()));
    assert!(researcher_tools.contains(&"grep_search".to_string()));
    assert!(researcher_tools.contains(&"glob".to_string()));
    assert!(researcher_tools.contains(&"delegate_task".to_string()));
    // Researcher must not have validator or manager tools
    assert!(!researcher_tools.contains(&"leave_verdict".to_string()));
    assert!(!researcher_tools.contains(&"create_plan".to_string()));
    assert!(!researcher_tools.contains(&"archive_current_plan".to_string()));

    // 2. Coder must have coding tools and no leave_verdict / create_plan
    let coder_entry = registry.resolve(Agent::Coder).expect("coder registered");
    let coder_tools: Vec<String> = all_tools
        .iter()
        .filter(|t| coder_entry.allows(&t.function.name))
        .map(|t| t.function.name.clone())
        .collect();

    assert!(coder_tools.contains(&"write_file".to_string()));
    assert!(coder_tools.contains(&"replace".to_string()));
    assert!(coder_tools.contains(&"read_file".to_string()));
    assert!(!coder_tools.contains(&"leave_verdict".to_string()));
    assert!(!coder_tools.contains(&"create_plan".to_string()));

    // 3. Validator must have inspection tools + leave_verdict
    let val_entry = registry
        .resolve(Agent::Validator)
        .expect("validator registered");
    let val_tools: Vec<String> = all_tools
        .iter()
        .filter(|t| val_entry.allows(&t.function.name))
        .map(|t| t.function.name.clone())
        .collect();

    assert!(val_tools.contains(&"leave_verdict".to_string()));
    assert!(val_tools.contains(&"read_file".to_string()));
    assert!(val_tools.contains(&"run_command".to_string()));
    assert!(val_tools.contains(&"grep_search".to_string()));
    assert!(val_tools.contains(&"glob".to_string()));
    assert!(!val_tools.contains(&"create_plan".to_string()));
    assert!(!val_tools.contains(&"archive_current_plan".to_string()));
}

#[test]
fn test_subagent_history_message_sequence() {
    let mut engine = marmennill::agent::ContextEngineFactory::new(128_000).specialist_context(
        "You are an expert coder.".to_string(),
        "Implement feature X.".to_string(),
    );

    // Initial messages: System + User (task brief)
    let msgs = engine.messages();
    assert_eq!(msgs.len(), 2);
    assert_eq!(msgs[0].role(), "system");
    assert_eq!(msgs[1].role(), "user");

    // Turn 1: Model calls read_file
    let tc1 = ToolCall {
        id: "call_abc123".to_string(),
        function: ToolFunction {
            name: "read_file".to_string(),
            arguments: r#"{"path":"src/main.rs"}"#.to_string(),
        },
    };
    engine.append(Message::Assistant {
        content: Some("Checking existing main.rs".to_string()),
        reasoning_content: None,
        tool_calls: vec![tc1.clone()],
    });

    // Tool response
    engine.append(Message::Tool {
        tool_call_id: tc1.id.clone(),
        content: "fn main() {}".to_string(),
    });

    let msgs = engine.messages();
    assert_eq!(msgs.len(), 4);
    assert_eq!(msgs[2].role(), "assistant");
    assert_eq!(msgs[3].role(), "tool");
    match &msgs[3] {
        Message::Tool {
            tool_call_id,
            content,
        } => {
            assert_eq!(tool_call_id, "call_abc123");
            assert_eq!(content, "fn main() {}");
        }
        _ => panic!("Expected Message::Tool"),
    }

    // Turn 2: Model finishes with MISSION COMPLETE
    engine.append(Message::Assistant {
        content: Some("Implemented feature X.\n\nMISSION COMPLETE".to_string()),
        reasoning_content: None,
        tool_calls: vec![],
    });

    let msgs = engine.messages();
    assert_eq!(msgs.len(), 5);
    assert_eq!(msgs[4].role(), "assistant");
}
