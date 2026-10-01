//! Integration tests for dynamic prompt builder, planner agent, and conditional validation.
//!
//! Verifies:
//! 1. Plan creation pre-generates prompt pairs (`<task_id>.md` and optional `<task_id>-validation.md`) on disk.
//! 2. Disk-first lookup in `delegate()` rehydrates exact saved prompt specifications.
//! 3. Planner agent blueprint includes `AGENTS.md` archetypes and strictly zero micro-skills.
//! 4. Project-level `skills/` and `AGENTS.md` override and augment catalog archetypes.
//! 5. All tests run offline against deterministic mock/canned data.

use marmennill::agents::{
    Agent, AgentBlueprint, Catalog, DelegationRequest, MissionMarker, PromptBuilder,
};
use marmennill::harness::HarnessStats;
use marmennill::llm::ChatClient;
use marmennill::manager::Plan;
use marmennill::orchestrator::OrchestratorManager;
use std::sync::Arc;

fn test_manager(tmp: &tempfile::TempDir) -> OrchestratorManager {
    OrchestratorManager::new(
        ChatClient::new("http://localhost:9999/v1", "test-model"),
        Plan::at(tmp.path().join(".marmel")),
        Arc::new(HarnessStats::new()),
    )
}

#[tokio::test]
async fn test_plan_creation_pregenerates_prompt_pairs_on_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);

    let plan_markdown = r#"# Execution Plan

### Phase 1: Research
- [ ] [t-001] Audit existing code structure (researcher)

### Phase 2: Implementation
- [ ] [t-002] Implement the token stream parser (coder)
- [ ] [t-003] Fix buffer overflow in string decoder (debugger)
"#;

    marmennill::harness::with_workspace_root(tmp.path(), async {
        mgr.create_plan(plan_markdown)
            .expect("plan creation succeeds");

        let prompts_dir = tmp.path().join(".marmel").join("prompts");
        assert!(prompts_dir.exists(), "prompts directory must be created");

        // Worker prompts must exist for all three tasks
        assert!(
            prompts_dir.join("t-001.md").exists(),
            "t-001.md worker prompt exists"
        );
        assert!(
            prompts_dir.join("t-002.md").exists(),
            "t-002.md worker prompt exists"
        );
        assert!(
            prompts_dir.join("t-003.md").exists(),
            "t-003.md worker prompt exists"
        );

        // Validation prompts: coder (t-002) and debugger (t-003) get validation; researcher (t-001) does not
        assert!(
            prompts_dir.join("t-002-validation.md").exists(),
            "t-002-validation.md must be generated for coder implementation"
        );
        assert!(
            prompts_dir.join("t-003-validation.md").exists(),
            "t-003-validation.md must be generated for debugger fix"
        );
        assert!(
            !prompts_dir.join("t-001-validation.md").exists(),
            "t-001-validation.md must NOT be generated for pure research"
        );

        // Verify content and frontmatter of pregenerated prompts
        let bp_coder = AgentBlueprint::load_from_disk(&prompts_dir.join("t-002.md")).unwrap();
        assert_eq!(bp_coder.role_name, "coder_specialist");
        assert!(bp_coder.allowed_tools.contains(&"write_file".to_string()));
        assert!(bp_coder.selected_skills.contains(&"clean_code".to_string()));
    })
    .await;
}

#[tokio::test]
async fn test_disk_first_delegation_loads_blueprint() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);
    let prompts_dir = tmp.path().join(".marmel").join("prompts");
    std::fs::create_dir_all(&prompts_dir).unwrap();

    // Manually write a bespoke prompt to disk
    let custom_bp = AgentBlueprint {
        role_name: "custom_parser_architect".to_string(),
        reasoning: "Pre-generated prompt for t-100".to_string(),
        selected_skills: vec!["clean_code".to_string(), "testing".to_string()],
        allowed_tools: vec![
            "read_file".to_string(),
            "write_file".to_string(),
            "rebirth".to_string(),
        ],
        system_prompt: "You are the Custom Parser Architect.".to_string(),
        task_id: Some("t-100".to_string()),
    };
    custom_bp.save_to_disk(&prompts_dir).unwrap();

    let req = DelegationRequest {
        agent_name: Agent::Coder,
        prompt: "Build parser according to blueprint".to_string(),
        snippets: vec![],
        task_id: Some("t-100".to_string()),
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };

    marmennill::harness::with_workspace_root(tmp.path(), async {
        let deliverable = mgr.delegate(req).await.expect("delegation succeeds");
        assert!(matches!(deliverable.marker, MissionMarker::Complete { .. }));
        assert!(deliverable.content.contains("Custom Parser Architect"));
    })
    .await;
}

#[tokio::test]
async fn test_planner_agent_blueprint_has_archetypes_and_zero_skills() {
    let tmp = tempfile::tempdir().unwrap();
    let catalog = Catalog::discover(tmp.path());

    let req = DelegationRequest {
        agent_name: Agent::Planner,
        prompt: "Decompose mission: build REST API".to_string(),
        snippets: vec![],
        task_id: None,
        image_urls: None,
        audio_urls: None,
        recursion_granted: false,
    };

    let blueprint = PromptBuilder::synthesize_offline(&catalog, &req);
    assert_eq!(blueprint.role_name, "planner");
    assert!(
        blueprint.selected_skills.is_empty(),
        "Planner must have 0 micro-skills"
    );
    assert!(blueprint.allowed_tools.contains(&"create_plan".to_string()));
    assert!(blueprint.allowed_tools.contains(&"read_file".to_string()));
    assert!(!blueprint.allowed_tools.contains(&"write_file".to_string()));

    // Must contain archetype catalog but not micro-skills
    assert!(
        blueprint
            .system_prompt
            .contains("Available Agent Archetypes")
    );
    assert!(!blueprint.system_prompt.contains("Active Domain Skills"));
}

#[tokio::test]
async fn test_catalog_project_level_archetypes_and_skills() {
    let tmp = tempfile::tempdir().unwrap();

    // 1. Create a custom project-level skill
    let skills_dir = tmp.path().join("skills");
    std::fs::create_dir_all(&skills_dir).unwrap();
    let custom_skill = r#"---
id: distributed_systems
name: Distributed Systems Patterns
description: Consensus, idempotency, and partition tolerance.
suggested_tools:
  - read_file
  - grep_search
---

## Distributed Systems Operational Guidance
Ensure all network RPCs are idempotent.
"#;
    std::fs::write(skills_dir.join("distributed.md"), custom_skill).unwrap();

    // 2. Create custom project-level AGENTS.md
    let custom_agents = r#"# Project Agent Archetypes

## Site Reliability Engineer
description: Resilient systems, fault tolerance, and chaos verification.
skills: distributed_systems, testing
tools: read_file, run_command, grep_search, glob, rebirth
"#;
    std::fs::write(tmp.path().join("AGENTS.md"), custom_agents).unwrap();

    // 3. Discover catalog
    let catalog = Catalog::discover(tmp.path());

    // Verify skill discovered
    let skill = catalog
        .get_skill("distributed_systems")
        .expect("custom skill loaded");
    assert_eq!(skill.name, "Distributed Systems Patterns");

    // Verify custom archetype discovered
    let arch = catalog
        .get_agent("site_reliability_engineer")
        .expect("custom archetype loaded");
    assert_eq!(arch.name, "Site Reliability Engineer");
    assert!(
        arch.default_skills
            .contains(&"distributed_systems".to_string())
    );
    assert!(arch.default_tools.contains(&"run_command".to_string()));
}
