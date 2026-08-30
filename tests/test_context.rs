//! Phase A integration tests (context engine & compaction).

use marmennill::agent::ContextEngineFactory;
use marmennill::types::Message;

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
