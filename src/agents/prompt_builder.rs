//! Dynamic Prompt Builder & Agent Architect.
//!
//! Designs a bespoke agent specification (role, least-privilege tools, and tailored system prompt)
//! Just-In-Time (JIT) before delegating a task. Persists all synthesized prompts to `.marmel/prompts/`.

use crate::agents::DelegationRequest;
use crate::agents::catalog::Catalog;
use crate::types::{ChatRequest, Message};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// The synthesized specification for an isolated worker subagent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentBlueprint {
    pub role_name: String,
    pub reasoning: String,
    pub selected_skills: Vec<String>,
    pub allowed_tools: Vec<String>,
    pub system_prompt: String,
    #[serde(default)]
    pub task_id: Option<String>,
}

impl AgentBlueprint {
    /// Save the synthesized prompt with YAML frontmatter to the given directory.
    pub fn save_to_disk(&self, dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let file_stem = self
            .task_id
            .as_deref()
            .map(|t| {
                t.trim_matches(|c| {
                    c == '[' || c == ']' || c == '(' || c == ')' || c == '"' || c == '\''
                })
                .trim()
            })
            .filter(|s| !s.is_empty())
            .unwrap_or(self.role_name.as_str());

        let filename = format!("{file_stem}.md");
        let dest = dir.join(filename);

        let created_at = chrono::Utc::now().to_rfc3339();
        let tools_yaml = self
            .allowed_tools
            .iter()
            .map(|t| format!("  - \"{t}\""))
            .collect::<Vec<_>>()
            .join("\n");
        let skills_yaml = self
            .selected_skills
            .iter()
            .map(|s| format!("  - \"{s}\""))
            .collect::<Vec<_>>()
            .join("\n");

        let content = format!(
            "---\nrole_name: \"{}\"\ntask_id: \"{}\"\ncreated_at: \"{}\"\nreasoning: \"{}\"\nselected_skills:\n{}\nallowed_tools:\n{}\n---\n\n{}",
            self.role_name,
            self.task_id.as_deref().unwrap_or(""),
            created_at,
            self.reasoning.replace('"', "\\\""),
            skills_yaml,
            tools_yaml,
            self.system_prompt
        );

        std::fs::write(&dest, content)?;
        Ok(dest)
    }

    /// Parse a synthesized prompt blueprint from raw markdown text with YAML frontmatter.
    pub fn parse_from_markdown(raw: &str) -> Result<Self> {
        let trimmed = raw.trim();

        if let Some(stripped) = trimmed.strip_prefix("---")
            && let Some(end_idx) = stripped.find("---")
        {
            let frontmatter = &stripped[..end_idx];
            let body = stripped[end_idx + 3..].trim().to_string();

            let mut role_name = "custom_specialist".to_string();
            let mut task_id = None;
            let mut reasoning = String::new();
            let mut selected_skills = Vec::new();
            let mut allowed_tools = Vec::new();

            let mut current_list = None;

            for line in frontmatter.lines() {
                let l = line.trim();
                if l.is_empty() {
                    continue;
                }
                if let Some(list_kind) = current_list {
                    if l.starts_with('-') || l.starts_with('*') {
                        let item = l
                            .trim_start_matches(['-', '*', ' '])
                            .trim()
                            .trim_matches('"');
                        if !item.is_empty() {
                            match list_kind {
                                1 => selected_skills.push(item.to_string()),
                                2 => allowed_tools.push(item.to_string()),
                                _ => {}
                            }
                        }
                        continue;
                    } else if !l.contains(':') {
                        let item = l.trim().trim_matches('"');
                        if !item.is_empty() {
                            match list_kind {
                                1 => selected_skills.push(item.to_string()),
                                2 => allowed_tools.push(item.to_string()),
                                _ => {}
                            }
                        }
                        continue;
                    } else {
                        current_list = None;
                    }
                }

                if let Some((k, v)) = l.split_once(':') {
                    let key = k.trim().to_ascii_lowercase();
                    let val = v.trim().trim_matches('"');
                    match key.as_str() {
                        "role_name" => role_name = val.to_string(),
                        "task_id" => {
                            if !val.is_empty() {
                                task_id = Some(val.to_string());
                            }
                        }
                        "reasoning" => reasoning = val.to_string(),
                        "selected_skills" => {
                            current_list = Some(1);
                        }
                        "allowed_tools" => {
                            current_list = Some(2);
                        }
                        _ => {}
                    }
                }
            }

            return Ok(AgentBlueprint {
                role_name,
                reasoning,
                selected_skills,
                allowed_tools,
                system_prompt: body,
                task_id,
            });
        }

        // Fallback: check if the markdown body contains explicit tool directives like
        // `- **ALLOWED TOOLS:** You are granted access to: ...` or `- Tools available: ...`
        let mut allowed_tools = Vec::new();
        for line in raw.lines() {
            let lower = line.to_ascii_lowercase();
            if lower.contains("allowed tools") || lower.contains("tools available") {
                for part in line.split('`') {
                    let token = part.trim().trim_matches([',', ' ', '.', '`']);
                    if !token.is_empty() && !token.contains(' ') {
                        let norm = crate::harness::normalize_tool_name(token);
                        if crate::types::ToolDef::default_tools()
                            .iter()
                            .any(|d| d.function.name == norm || d.function.name == token)
                            && !allowed_tools.contains(&token.to_string())
                        {
                            allowed_tools.push(token.to_string());
                        }
                    }
                }
            }
        }

        if !allowed_tools.is_empty() {
            return Ok(AgentBlueprint {
                role_name: "custom_specialist".to_string(),
                reasoning: "Parsed from inline prompt tool directives".to_string(),
                selected_skills: Vec::new(),
                allowed_tools,
                system_prompt: raw.to_string(),
                task_id: None,
            });
        }

        anyhow::bail!("invalid prompt format: missing frontmatter or tool specification")
    }

    /// Load an existing synthesized prompt blueprint from disk.
    pub fn load_from_disk(path: &Path) -> Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("reading prompt file {}", path.display()))?;
        Self::parse_from_markdown(&raw)
            .with_context(|| format!("parsing prompt file {}", path.display()))
    }
}

/// Prompt Builder: synthesizes an [`AgentBlueprint`] dynamically.
pub struct PromptBuilder;

impl PromptBuilder {
    /// Build an [`AgentBlueprint`] for a delegation request, optionally persisting to disk.
    pub async fn build_blueprint(
        client: Option<&crate::llm::ChatClient>,
        model: Option<&str>,
        catalog: &Catalog,
        req: &DelegationRequest,
        prompts_dir: Option<&Path>,
    ) -> AgentBlueprint {
        let is_test_runner = std::env::current_exe()
            .map(|p| {
                let s = p.to_string_lossy();
                s.contains("/deps/") || s.contains(r"\deps\")
            })
            .unwrap_or(false)
            && std::env::var("MARMEL_LIVE_TEST").is_err();

        let blueprint = if req.agent_name == crate::agents::Agent::Planner || is_test_runner {
            Self::synthesize_offline(catalog, req)
        } else if let (Some(c), Some(m)) = (client, model) {
            match Self::synthesize_with_llm(c, m, catalog, req).await {
                Ok(bp) => bp,
                Err(e) => {
                    tracing::warn!(
                        "Dynamic prompt synthesis via LLM failed, using offline synthesizer: {e}"
                    );
                    Self::synthesize_offline(catalog, req)
                }
            }
        } else {
            Self::synthesize_offline(catalog, req)
        };

        if let Some(dir) = prompts_dir
            && let Err(e) = blueprint.save_to_disk(dir)
        {
            tracing::warn!("Failed to persist synthesized prompt to disk: {e}");
        }

        blueprint
    }

    /// JIT synthesis using the Agent Architect LLM prompt.
    pub async fn synthesize_with_llm(
        client: &crate::llm::ChatClient,
        model: &str,
        catalog: &Catalog,
        req: &DelegationRequest,
    ) -> Result<AgentBlueprint> {
        let catalog_context = catalog.format_catalog_for_prompt();
        let snippets_context = if req.snippets.is_empty() {
            "None provided".to_string()
        } else {
            req.snippets.join("\n---\n")
        };

        let user_prompt = format!(
            "### Incoming Delegation Request\n- **Task ID:** {}\n- **Agent Archetype Hint:** {}\n- **Task Brief:**\n{}\n\n- **Context Snippets:**\n{}\n\n{}",
            req.task_id.as_deref().unwrap_or("unassigned"),
            req.agent_name,
            req.prompt,
            snippets_context,
            catalog_context
        );

        let chat_req = ChatRequest {
            model: model.to_string(),
            messages: vec![
                Message::System {
                    content: crate::prompts::PROMPT_BUILDER_PROMPT.to_string(),
                },
                Message::User {
                    content: user_prompt,
                },
            ],
            temperature: Some(0.1),
            top_p: None,
            frequency_penalty: None,
            presence_penalty: None,
            stream: Some(false),
            enable_thinking: Some(false),
            tools: None,
        };

        let mut raw_response = String::new();
        let reply = client
            .chat_stream(&chat_req, |chunk| {
                raw_response.push_str(chunk);
                true
            })
            .await
            .context("calling chat_stream for prompt architect")?;

        let raw = if !reply.content.trim().is_empty() {
            reply.content.trim()
        } else if !reply.raw.trim().is_empty() {
            reply.raw.trim()
        } else {
            raw_response.trim()
        };

        let json_text = if let Some(stripped) = raw.strip_prefix("```json") {
            stripped.trim_end_matches("```").trim()
        } else if let Some(stripped) = raw.strip_prefix("```") {
            stripped.trim_end_matches("```").trim()
        } else if let Some(start) = raw.find('{') {
            if let Some(end) = raw.rfind('}') {
                &raw[start..=end]
            } else {
                raw
            }
        } else {
            raw
        };

        #[derive(Deserialize)]
        struct ArchitectOutput {
            pub role_name: String,
            pub reasoning: String,
            pub selected_skills: Vec<String>,
            pub allowed_tools: Vec<String>,
            pub system_prompt: String,
        }

        let parsed: ArchitectOutput = serde_json::from_str(json_text)
            .with_context(|| format!("parsing architect output: {json_text}"))?;

        // Sanitize tools: ensure rebirth is present if non-trivial tools are included
        let mut tools = parsed.allowed_tools;
        if !tools.iter().any(|t| t == "rebirth") {
            tools.push("rebirth".to_string());
        }

        Ok(AgentBlueprint {
            role_name: parsed.role_name,
            reasoning: parsed.reasoning,
            selected_skills: parsed.selected_skills,
            allowed_tools: tools,
            system_prompt: parsed.system_prompt,
            task_id: req.task_id.clone(),
        })
    }

    /// Fast, deterministic, zero-network synthesis combining matching catalog skills and archetype defaults.
    pub fn synthesize_offline(catalog: &Catalog, req: &DelegationRequest) -> AgentBlueprint {
        if req.agent_name == crate::agents::Agent::Planner {
            let role_name = "planner".to_string();
            let mut system_prompt = crate::prompts::PLANNER_PROMPT.to_string();
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&catalog.format_agents_for_planner());
            system_prompt.push_str("\n\n");
            system_prompt.push_str(&crate::prompts::format_environment_block());
            return AgentBlueprint {
                role_name,
                reasoning: "Strategic Planner configured with AGENTS archetypes and strictly zero micro-skills".to_string(),
                selected_skills: Vec::new(),
                allowed_tools: vec![
                    "read_file".to_string(),
                    "grep_search".to_string(),
                    "glob".to_string(),
                    "create_plan".to_string(),
                    "rebirth".to_string(),
                ],
                system_prompt,
                task_id: req.task_id.clone(),
            };
        }

        let archetype = catalog.get_agent(req.agent_name.as_str());
        let prompt_lower = req.prompt.to_ascii_lowercase();

        let mut selected_skills = Vec::new();
        if let Some(arch) = archetype {
            for s in &arch.default_skills {
                if !selected_skills.contains(s) {
                    selected_skills.push(s.clone());
                }
            }
        }

        // Domain keyword heuristics
        let is_debug = prompt_lower.contains("bug")
            || prompt_lower.contains("crash")
            || prompt_lower.contains("error")
            || prompt_lower.contains("panic")
            || prompt_lower.contains("segfault")
            || prompt_lower.contains("fault")
            || prompt_lower.contains("fix");
        let is_testing = prompt_lower.contains("test")
            || prompt_lower.contains("verify")
            || prompt_lower.contains("assert")
            || prompt_lower.contains("coverage");
        let is_research = prompt_lower.contains("research")
            || prompt_lower.contains("search")
            || prompt_lower.contains("find")
            || prompt_lower.contains("document")
            || prompt_lower.contains("spec");
        let is_coding = prompt_lower.contains("code")
            || prompt_lower.contains("implement")
            || prompt_lower.contains("refactor")
            || prompt_lower.contains("create")
            || prompt_lower.contains("write");

        if is_debug && !selected_skills.iter().any(|s| s == "debugging") {
            selected_skills.push("debugging".to_string());
        }
        if is_testing && !selected_skills.iter().any(|s| s == "testing") {
            selected_skills.push("testing".to_string());
        }
        if is_research && !selected_skills.iter().any(|s| s == "research") {
            selected_skills.push("research".to_string());
        }
        if is_coding && !selected_skills.iter().any(|s| s == "clean_code") {
            selected_skills.push("clean_code".to_string());
        }

        if selected_skills.is_empty() {
            selected_skills.push("clean_code".to_string());
        }

        // Gather allowed tools from selected skills and archetype
        let mut allowed_tools = Vec::new();
        if let Some(arch) = archetype {
            for t in &arch.default_tools {
                if !allowed_tools.contains(t) {
                    allowed_tools.push(t.clone());
                }
            }
        }

        for s_id in &selected_skills {
            if let Some(skill) = catalog.get_skill(s_id) {
                for t in &skill.suggested_tools {
                    if !allowed_tools.contains(t) {
                        allowed_tools.push(t.clone());
                    }
                }
            }
        }

        if !allowed_tools.iter().any(|t| t == "rebirth") {
            allowed_tools.push("rebirth".to_string());
        }

        // Assemble synthesized prompt
        let role_name = format!("{}_specialist", req.agent_name.as_str());
        let mut system_prompt = format!(
            "# Marmel: {}\n\n**Mission:** You are a dynamically synthesized specialist dedicated to executing the delegated task with surgical precision.\n\n",
            role_name
        );

        system_prompt.push_str("## Active Domain Skills\n");
        for s_id in &selected_skills {
            if let Some(skill) = catalog.get_skill(s_id) {
                system_prompt
                    .push_str(&format!("### Skill: {}\n{}\n\n", skill.name, skill.content));
            }
        }

        system_prompt.push_str("## Strict Operational Discipline\n");
        system_prompt.push_str(&format!(
            "- **ALLOWED TOOLS:** You are granted access to: `{}`.\n",
            allowed_tools.join("`, `")
        ));
        system_prompt.push_str("- **TASK SCOPE:** Execute ONLY the assigned task. Do NOT take over planning or subsequent tasks.\n");
        system_prompt.push_str(
            "- **ZERO HALLUCINATION:** Inspect real code and verify actual command outputs.\n",
        );
        system_prompt.push_str("- **TERMINAL MARKERS:** Always conclude with `MISSION COMPLETE` upon successful completion, or `FAILED: <reason>` if impossible.\n");
        system_prompt.push_str("- **LANGUAGE:** You MUST respond and deliver in English only.\n");

        AgentBlueprint {
            role_name,
            reasoning: format!(
                "Synthesized offline based on archetype `{}` and task keywords",
                req.agent_name
            ),
            selected_skills,
            allowed_tools,
            system_prompt,
            task_id: req.task_id.clone(),
        }
    }

    /// Parse all task items from the raw execution plan markdown.
    pub fn parse_tasks_from_plan(markdown: &str) -> Vec<PlanTaskItem> {
        static PLAN_TASK_RE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
            regex::Regex::new(
                r"(?mi)^\s*[-*]\s*\[\s*\]\s*\*{0,2}\[?(t-[A-Za-z0-9_-]+)\]?\*{0,2}\s*(.*?)$",
            )
            .expect("valid plan task regex")
        });

        let mut items = Vec::new();
        for cap in PLAN_TASK_RE.captures_iter(markdown) {
            let task_id = cap[1].to_string();
            let raw_desc = cap[2].trim();

            let (description, role_hint) = if let Some(open) = raw_desc.rfind('(') {
                if raw_desc.ends_with(')') {
                    let role_candidate = raw_desc[open + 1..raw_desc.len() - 1]
                        .trim()
                        .to_ascii_lowercase();
                    (raw_desc[..open].trim().to_string(), Some(role_candidate))
                } else {
                    (raw_desc.to_string(), None)
                }
            } else {
                (raw_desc.to_string(), None)
            };

            items.push(PlanTaskItem {
                task_id,
                description,
                role_hint,
            });
        }
        items
    }

    /// Pre-generate prompt pairs (<task_id>.md and optional <task_id>-validation.md) for all tasks in a plan.
    pub fn pregenerate_for_plan_offline(
        plan_markdown: &str,
        catalog: &Catalog,
        prompts_dir: &Path,
    ) -> Vec<String> {
        let _ = std::fs::create_dir_all(prompts_dir);
        let tasks = Self::parse_tasks_from_plan(plan_markdown);
        let mut generated = Vec::new();

        for item in tasks {
            let agent = item
                .role_hint
                .as_deref()
                .and_then(crate::agents::Agent::from_str)
                .unwrap_or(crate::agents::Agent::Coder);

            let req = DelegationRequest {
                agent_name: agent,
                prompt: item.description.clone(),
                snippets: Vec::new(),
                task_id: Some(item.task_id.clone()),
                image_urls: None,
                audio_urls: None,
                recursion_granted: false,
            };

            // 1. Worker prompt
            let worker_bp = Self::synthesize_offline(catalog, &req);
            if let Ok(path) = worker_bp.save_to_disk(prompts_dir) {
                generated.push(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                );
            }

            // 2. Validation prompt (optional)
            let desc_lower = item.description.to_ascii_lowercase();
            let is_doc_or_config = desc_lower.contains("readme")
                || desc_lower.contains("docs")
                || desc_lower.contains("documentation")
                || desc_lower.contains("markdown")
                || desc_lower.contains("report")
                || desc_lower.contains("summary")
                || desc_lower.contains("summariz");

            let needs_validation = !is_doc_or_config
                && (agent == crate::agents::Agent::Coder
                    || agent == crate::agents::Agent::Debugger
                    || desc_lower.contains("implement")
                    || desc_lower.contains("refactor")
                    || desc_lower.contains("fix")
                    || desc_lower.contains("patch")
                    || (desc_lower.contains("build") && !desc_lower.contains("verify"))
                    || desc_lower.contains("unit test"));

            if needs_validation
                && agent != crate::agents::Agent::Planner
                && agent != crate::agents::Agent::Validator
            {
                let val_filename = format!("{}-validation.md", item.task_id);
                let val_dest = prompts_dir.join(val_filename.clone());
                let val_content = format!(
                    "---\nrole_name: \"validator\"\ntask_id: \"{}\"\ntarget_brief: \"{}\"\ncreated_at: \"{}\"\nallowed_tools:\n  - \"read_file\"\n  - \"grep_search\"\n  - \"glob\"\n  - \"run_command\"\n  - \"leave_verdict\"\n  - \"rebirth\"\n---\n\n# Independent Quality Verification for {}\n\n**Mission:** Independently audit the deliverable for task `{}`.\n**Brief:** {}\n\n## Verification Protocol:\n1. Inspect created or modified files to verify correctness and conformance.\n2. Execute test suites or checks if applicable.\n3. Conclude by calling `leave_verdict(verdict=\"APPROVED\" | \"REJECTED\", comments=\"...\")`.\n",
                    item.task_id,
                    item.description.replace('"', "\\\""),
                    chrono::Utc::now().to_rfc3339(),
                    item.task_id,
                    item.task_id,
                    item.description
                );
                if std::fs::write(&val_dest, val_content).is_ok() {
                    generated.push(val_filename);
                }
            }
        }

        generated
    }
}

/// A parsed task item from the execution plan.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanTaskItem {
    pub task_id: String,
    pub description: String,
    pub role_hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Agent;

    #[test]
    fn test_offline_synthesizer() {
        let catalog = Catalog::new();
        let req = DelegationRequest {
            agent_name: Agent::Coder,
            prompt: "Fix crash in buffer handling and run tests".to_string(),
            snippets: vec!["src/buffer.rs".to_string()],
            task_id: Some("t-001".to_string()),
            image_urls: None,
            audio_urls: None,
            recursion_granted: false,
        };

        let blueprint = PromptBuilder::synthesize_offline(&catalog, &req);
        assert_eq!(blueprint.task_id.as_deref(), Some("t-001"));
        assert!(
            blueprint
                .selected_skills
                .contains(&"clean_code".to_string())
        );
        assert!(blueprint.selected_skills.contains(&"debugging".to_string()));
        assert!(blueprint.selected_skills.contains(&"testing".to_string()));
        assert!(blueprint.allowed_tools.contains(&"run_command".to_string()));
        assert!(blueprint.allowed_tools.contains(&"replace".to_string()));
        assert!(blueprint.system_prompt.contains("Active Domain Skills"));
    }

    #[test]
    fn test_save_and_load_blueprint() {
        let tmp = tempfile::tempdir().unwrap();
        let bp = AgentBlueprint {
            role_name: "rust_refactorer".to_string(),
            reasoning: "Unit test blueprint".to_string(),
            selected_skills: vec!["clean_code".to_string()],
            allowed_tools: vec![
                "read_file".to_string(),
                "write_file".to_string(),
                "rebirth".to_string(),
            ],
            system_prompt: "You are a test agent.".to_string(),
            task_id: Some("t-042".to_string()),
        };

        let path = bp.save_to_disk(tmp.path()).unwrap();
        assert!(path.exists());
        assert_eq!(path.file_name().unwrap(), "t-042.md");

        let loaded = AgentBlueprint::load_from_disk(&path).unwrap();
        assert_eq!(loaded.role_name, "rust_refactorer");
        assert_eq!(loaded.task_id.as_deref(), Some("t-042"));
        assert_eq!(loaded.selected_skills, vec!["clean_code"]);
        assert_eq!(
            loaded.allowed_tools,
            vec!["read_file", "write_file", "rebirth"]
        );
        assert_eq!(loaded.system_prompt, "You are a test agent.");
    }

    #[test]
    fn test_pregenerate_for_plan_offline() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let plan = r#"
# Execution Plan

### Phase 1: Research
- [ ] [t-001] Investigate API contracts (researcher)

### Phase 2: Implementation
- [ ] [t-002] Implement parser logic (coder)

### Phase 3: Documentation
- [ ] [t-003] Produce documentation in README.md (generalist)
"#;
        let generated = PromptBuilder::pregenerate_for_plan_offline(plan, &catalog, tmp.path());
        assert!(generated.contains(&"t-001.md".to_string()));
        assert!(generated.contains(&"t-002.md".to_string()));
        assert!(generated.contains(&"t-003.md".to_string()));
        // t-002 is a coder task ("implement"), so it must also generate t-002-validation.md
        assert!(generated.contains(&"t-002-validation.md".to_string()));
        // t-001 is pure research and t-003 is documentation, so neither should have validation
        assert!(!generated.contains(&"t-001-validation.md".to_string()));
        assert!(!generated.contains(&"t-003-validation.md".to_string()));
    }

    #[test]
    fn test_planner_synthesis_has_no_micro_skills_and_includes_archetypes() {
        let catalog = Catalog::new();
        let req = DelegationRequest {
            agent_name: Agent::Planner,
            prompt: "Formulate an execution plan for building the authentication module"
                .to_string(),
            snippets: Vec::new(),
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
        assert!(
            blueprint
                .system_prompt
                .contains("Available Agent Archetypes")
        );
        assert!(!blueprint.system_prompt.contains("Active Domain Skills"));
    }
}
