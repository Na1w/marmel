//! Integration tests for dynamic prompt builder, planner agent, and conditional validation.
//!
//! Verifies:
//! 1. Plan creation generates prompt pairs (`<task_id>.md` and optional
//!    `<task_id>-validation.md`) on disk in advance, and records the *omission* of a
//!    validation prompt durably as `<task_id>-validation-skip.md` for the tasks it
//!    designates validation-free (research / documentation work).
//! 2. Disk-first lookup in `delegate()` restores the exact saved prompt specifications.
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
- [ ] [t-001] Audit existing code structure (no-validate) (researcher)
- [ ] [t-004] Update the README with the new validation section (no-validate) (researcher)

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

        // Validation prompts are decided by the plan line's directive: coder (t-002)
        // and debugger (t-003) carry no directive (`Auto`) and are validated anyway,
        // while the researcher lines declared `(no-validate)`.
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
            "t-001-validation.md must NOT be generated for a line declaring `(no-validate)`"
        );

        // Omitting the validation prompt is a **decision made by the plan line**, so
        // `create_plan` records it on disk as `<task_id>-validation-skip.md` (one-line
        // reason) next to the worker prompt. Without that record the runner would read
        // the absent brief as a verdict gap and report the task FAILED.
        for skipped in ["t-001", "t-004"] {
            let record = prompts_dir.join(format!("{skipped}-validation-skip.md"));
            assert!(
                record.exists(),
                "{skipped} must carry a `-validation-skip.md` record for its omitted validation prompt"
            );
            assert_eq!(
                std::fs::read_to_string(&record)
                    .expect("skip record")
                    .trim(),
                "Plan directive (no-validate): validation not required",
                "the record must carry the one-line reason naming the plan directive"
            );
        }
        assert!(
            !prompts_dir.join("t-004-validation.md").exists(),
            "t-004-validation.md must NOT be generated for README documentation work"
        );
        // The two records are mutually exclusive: a task that got a validation brief
        // carries no skip record, so the read side can never see contradictory signals.
        for validated in ["t-002", "t-003"] {
            assert!(
                !prompts_dir
                    .join(format!("{validated}-validation-skip.md"))
                    .exists(),
                "{validated} is validated by design and must carry no skip record"
            );
        }

        let bp_coder = AgentBlueprint::load_from_disk(&prompts_dir.join("t-002.md")).unwrap();
        assert_eq!(bp_coder.role_name, "coder_specialist");
        assert!(bp_coder.allowed_tools.contains(&"write_file".to_string()));
        assert!(bp_coder.selected_skills.contains(&"clean_code".to_string()));
    })
    .await;
}

/// The two write-side records stay mutually exclusive across **re-planning**: when a
/// task id whose line declared `(no-validate)` is re-planned with `(validate)`,
/// `create_plan` writes its `-validation.md` brief and deletes the stale record, so
/// the read side can never see contradictory signals.
#[tokio::test]
async fn test_replanning_a_task_as_validated_drops_its_stale_skip_record() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);

    let doc_plan = "# Execution Plan\n\n### Phase 1: Documentation\n\
                    - [ ] [t-005] Summarize the findings (no-validate) (researcher)\n";
    let impl_plan = "# Execution Plan\n\n### Phase 1: Implementation\n\
                     - [ ] [t-005] Implement the token stream parser (validate) (coder)\n";

    marmennill::harness::with_workspace_root(tmp.path(), async {
        let prompts_dir = tmp.path().join(".marmel").join("prompts");

        mgr.create_plan(doc_plan)
            .expect("first plan creation succeeds");
        assert!(
            prompts_dir.join("t-005-validation-skip.md").exists(),
            "t-005 must carry a skip record while it is documentation work"
        );
        assert!(
            !prompts_dir.join("t-005-validation.md").exists(),
            "t-005 must carry no validation brief while it is documentation work"
        );

        mgr.create_plan(impl_plan).expect("re-plan succeeds");
        assert!(
            prompts_dir.join("t-005-validation.md").exists(),
            "t-005 must get a validation brief once re-planned as implementation work"
        );
        assert!(
            !prompts_dir.join("t-005-validation-skip.md").exists(),
            "the stale skip record must be deleted together with the flipped decision"
        );
    })
    .await;
}

/// The opposite re-plan flip: a task id that was validated under the first plan and
/// whose new line declares `(no-validate)` must lose the **stale brief** of the
/// previous plan. The read side ranks a readable `-validation.md` above the record,
/// so leaving that file behind makes the next run audit the deliverable against the
/// brief of a different plan.
#[tokio::test]
async fn test_replanning_a_task_as_no_validate_drops_its_stale_validation_brief() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);

    let impl_plan = "# Execution Plan\n\n### Phase 1: Implementation\n\
                     - [ ] [t-006] Implement the token stream parser (validate) (coder)\n";
    let doc_plan = "# Execution Plan\n\n### Phase 1: Documentation\n\
                    - [ ] [t-006] Summarize the findings (no-validate) (researcher)\n";

    marmennill::harness::with_workspace_root(tmp.path(), async {
        let prompts_dir = tmp.path().join(".marmel").join("prompts");

        mgr.create_plan(impl_plan)
            .expect("first plan creation succeeds");
        assert!(
            prompts_dir.join("t-006-validation.md").exists(),
            "t-006 must carry a validation brief while its line asks for validation"
        );
        assert!(
            !prompts_dir.join("t-006-validation-skip.md").exists(),
            "t-006 must carry no skip record while its line asks for validation"
        );

        mgr.create_plan(doc_plan).expect("re-plan succeeds");
        assert!(
            !prompts_dir.join("t-006-validation.md").exists(),
            "the stale validation brief must be deleted together with the `(no-validate)` flip"
        );
        assert!(
            prompts_dir.join("t-006-validation-skip.md").exists(),
            "t-006 must carry a skip record once its line declares `(no-validate)`"
        );
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

/// A **reversed** directive spelling — `(researcher) (no-validate)` — is NOT a
/// directive: [`crate::plan_parse`] scans the trailing paren groups right to left,
/// so the reserved token must sit left of the archetype hint. The point of this
/// fixture is that a syntax error is **safe**: the line stays `Auto`, which keeps
/// validation ON and authors the auditor brief. A mistyped `(no-validate)` can
/// therefore never silently disable validation, and it never becomes a role hint
/// either (the closed archetype set refuses the reserved token).
#[tokio::test]
async fn test_a_reversed_directive_order_is_inert_and_still_authors_a_brief() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);

    let reversed_line = "- [ ] [t-010] Audit the vendor API (researcher) (no-validate)";
    let plan_markdown = format!("# Execution Plan\n\n### Phase 1: Research\n{reversed_line}\n");

    marmennill::harness::with_workspace_root(tmp.path(), async {
        // Grammar authority first: the reversed pair resolves to `Auto` and the
        // reserved token is neither a role hint nor a directive.
        let task = marmennill::plan_parse::parse_task_line(reversed_line).expect("task line");
        assert_eq!(
            task.validation_directive,
            marmennill::plan_parse::ValidationDirective::Auto,
            "a `(no-validate)` group right of the archetype hint is not a directive"
        );
        assert_eq!(
            task.role_hint, None,
            "the reserved token is never a role hint either"
        );

        // The write side consumes exactly that verdict — no policy of its own.
        let items = PromptBuilder::parse_tasks_from_plan(&plan_markdown);
        assert_eq!(items.len(), 1, "the line is still a pending task");
        assert_eq!(items[0].task_id, "t-010");
        assert_eq!(
            items[0].validation_directive,
            marmennill::plan_parse::ValidationDirective::Auto,
            "the inert spelling reaches the writer as `Auto`"
        );

        mgr.create_plan(&plan_markdown)
            .expect("plan creation succeeds");

        let prompts_dir = tmp.path().join(".marmel").join("prompts");
        assert!(
            prompts_dir.join("t-010-validation.md").exists(),
            "an inert `(no-validate)` placement must keep validation on"
        );
        assert!(
            !prompts_dir.join("t-010-validation-skip.md").exists(),
            "and it must never leave a skip record behind"
        );

        // The prose — reserved-looking token and all — is what the auditor is
        // briefed on, so nothing was stripped out of the mission either.
        let brief = std::fs::read_to_string(prompts_dir.join("t-010-validation.md"))
            .expect("authored validation brief");
        assert!(
            brief.contains("Independent Quality Verification for t-010"),
            "the brief names the task it audits:\n{brief}"
        );
        assert!(
            brief.contains("Audit the vendor API (researcher) (no-validate)"),
            "the unrecognised groups stay prose inside the briefed mission:\n{brief}"
        );

        let disk = std::fs::read_to_string(tmp.path().join(".marmel").join("execution_plan.md"))
            .expect("plan file");
        assert!(
            disk.contains(reversed_line),
            "the plan line is stored verbatim, token included:\n{disk}"
        );
    })
    .await;
}

/// A directive-shaped token **inside** the description (`Ship (no-validate) the
/// parser (coder)`) is prose: only a trailing group left of the role hint is the
/// reserved directive. So the line stays `Auto`, its brief is authored, no skip
/// record is written, and the wording of the description does not decide anything.
#[tokio::test]
async fn test_a_directive_token_inside_the_description_does_not_disable_validation() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);

    let plan_markdown = "# Execution Plan\n\n### Phase 1: Implementation\n\
                         - [ ] [t-011] Ship (no-validate) the parser (coder)\n";

    marmennill::harness::with_workspace_root(tmp.path(), async {
        let items = PromptBuilder::parse_tasks_from_plan(plan_markdown);
        assert_eq!(items.len(), 1, "the line is still a pending task");
        assert_eq!(
            items[0].validation_directive,
            marmennill::plan_parse::ValidationDirective::Auto,
            "a mid-description reserved token is prose, never a directive"
        );
        assert_eq!(
            items[0].role_hint.as_deref(),
            Some("coder"),
            "the trailing archetype hint is still read"
        );
        assert_eq!(
            items[0].description, "Ship (no-validate) the parser",
            "the prose token survives verbatim in the briefed description"
        );

        mgr.create_plan(plan_markdown)
            .expect("plan creation succeeds");

        let prompts_dir = tmp.path().join(".marmel").join("prompts");
        assert!(
            prompts_dir.join("t-011-validation.md").exists(),
            "prose wording cannot switch validation off: the brief must be authored"
        );
        assert!(
            !prompts_dir.join("t-011-validation-skip.md").exists(),
            "and no skip record may be written for a prose token"
        );
    })
    .await;
}

/// A directive placed **before** the task id (`- [ ] (no-validate) [t-012] …`) is
/// fail-closed on both counts: the reserved token occupies the id slot, so the
/// strict id recogniser refuses it and the line yields no task id at all. The
/// write side therefore authors neither a worker prompt, nor a brief, nor — most
/// importantly — a skip record, which is the only thing that could have silenced
/// validation. The malformed box is still an unchecked checklist item, so the plan
/// never reports itself complete.
#[tokio::test]
async fn test_a_directive_before_the_task_id_authors_neither_prompt_nor_record() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);

    let malformed = "- [ ] (no-validate) [t-012] Rework the parser (coder)";
    let plan_markdown = format!(
        "# Execution Plan\n\n### Phase 1: Implementation\n{malformed}\n\
         - [ ] [t-014] Land the sibling change (coder)\n"
    );

    marmennill::harness::with_workspace_root(tmp.path(), async {
        let task =
            marmennill::plan_parse::parse_task_line(malformed).expect("still a checkbox line");
        assert_eq!(
            task.task_id, None,
            "the reserved token in the id slot is never an id"
        );
        assert_eq!(
            task.validation_directive,
            marmennill::plan_parse::ValidationDirective::Auto,
            "a pre-id group is not a trailing group, so it is not a directive"
        );

        // Only the canonical sibling reaches the writer at all.
        let items = PromptBuilder::parse_tasks_from_plan(&plan_markdown);
        assert_eq!(
            items.iter().map(|i| i.task_id.as_str()).collect::<Vec<_>>(),
            vec!["t-014"],
            "an id-less line contributes no task item, so it can author nothing"
        );

        mgr.create_plan(&plan_markdown)
            .expect("a plan with one malformed line is still accepted");

        let prompts_dir = tmp.path().join(".marmel").join("prompts");
        for name in [
            "t-012.md",
            "t-012-validation.md",
            "t-012-validation-skip.md",
        ] {
            assert!(
                !prompts_dir.join(name).exists(),
                "a refused task id must author neither prompt nor record: {name} exists"
            );
        }
        // Non-vacuity: the writer did run for the same plan.
        assert!(
            prompts_dir.join("t-014.md").exists()
                && prompts_dir.join("t-014-validation.md").exists(),
            "the sibling line still gets its normal prompt pair"
        );

        let plan = Plan::at(tmp.path().join(".marmel"));
        let disk = std::fs::read_to_string(plan.plan_path()).expect("plan file");
        assert!(disk.contains(malformed), "line kept verbatim:\n{disk}");
        assert_eq!(
            plan.pending_tasks(),
            vec!["t-014".to_string()],
            "the malformed line yields no task id to pending_tasks"
        );
        assert_eq!(
            marmennill::plan_parse::pending_lines(&disk).len(),
            2,
            "both boxes stay unchecked checklist items — fail closed, never 'done'"
        );
        assert!(
            !plan.is_complete(),
            "a malformed box must never self-complete"
        );
    })
    .await;
}

/// The plan-progress summary is model-facing: it becomes
/// `SteerContext::plan_progress` for the steer arbitrator
/// (`ui::bridge::arbiter` fills it through `ui::format_plan_progress_summary`).
/// Echoing the reserved token there would only invite the model to imitate it, so
/// the renderer **drops** the directive while keeping the id and the role hint —
/// the on-disk plan line keeps every byte.
#[tokio::test]
async fn test_plan_progress_summary_never_echoes_the_directive_it_honoured() {
    let tmp = tempfile::tempdir().unwrap();
    let mgr = test_manager(&tmp);

    let plan_markdown = "# Execution Plan\n\n### Phase 1: Implementation\n\
                         - [ ] [t-013] Ship the parser behind a flag (no-validate) (coder)\n\
                         - [ ] [t-015] Fix the retry loop (validate) (coder)\n";

    marmennill::harness::with_workspace_root(tmp.path(), async {
        mgr.create_plan(plan_markdown)
            .expect("plan creation succeeds");

        // Both directives were honoured on disk by the write side…
        let prompts_dir = tmp.path().join(".marmel").join("prompts");
        assert!(prompts_dir.join("t-013-validation-skip.md").exists());
        assert!(prompts_dir.join("t-015-validation.md").exists());

        // …and one of those tasks has since finished, so the summary renders both
        // a completed and a pending step that each honoured a directive.
        let plan = Plan::at(tmp.path().join(".marmel"));
        assert!(
            plan.check_off("t-013")
                .expect("check-off is not an IO error"),
            "the directive-bearing line is checkable by its id"
        );

        // …and neither token may reappear in the rendered progress block.
        let disk = std::fs::read_to_string(tmp.path().join(".marmel").join("execution_plan.md"))
            .expect("plan file");
        assert!(disk.contains("(no-validate)") && disk.contains("(validate)"));

        let summary = marmennill::orchestrator::generate_plan_progress_summary(&disk);
        assert!(
            !summary.to_lowercase().contains("valid"),
            "the summary must not echo the reserved directive it acted on:\n{summary}"
        );
        assert_eq!(
            summary,
            marmennill::ui::format_plan_progress_summary(&disk),
            "the UI seam the arbitrator feeds on is the same renderer, not a second one"
        );
        assert!(
            summary.contains("[t-013] Ship the parser behind a flag (coder)"),
            "the id and the routing hint stay in the summary:\n{summary}"
        );
        assert!(
            summary.contains("[t-015] Fix the retry loop (coder)"),
            "and so does the `(validate)` line, minus its token:\n{summary}"
        );
        assert!(
            summary.contains("### Completed Steps (1/2):")
                && summary.contains("### Pending Steps (1/2):")
                && summary.contains("Overall Progress: 1/2 tasks completed (50%)"),
            "the directive must not disturb the counts:\n{summary}"
        );
    })
    .await;
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

    let catalog = Catalog::discover(tmp.path());

    let skill = catalog
        .get_skill("distributed_systems")
        .expect("custom skill loaded");
    assert_eq!(skill.name, "Distributed Systems Patterns");

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
