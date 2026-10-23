//! Dynamic Prompt Builder & Agent Architect.
//!
//! Designs a bespoke agent specification (role, least-privilege tools, and tailored system prompt)
//! Just-In-Time (JIT) before delegating a task. Persists all synthesized prompts to `.marmel/prompts/`.

use crate::agents::DelegationRequest;
use crate::agents::catalog::Catalog;
use crate::markers::{MARKER_COMPLETE, MARKER_FAILED};
use crate::orchestrator::pool;
use crate::plan_parse::ValidationDirective;
use crate::tool_names::{
    TOOL_CREATE_PLAN, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_READ_FILE, TOOL_REBIRTH,
};
use crate::types::{ChatRequest, Message};
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Suffix of the durable record stating that a task **needs no validation**.
///
/// Written next to the prompts it belongs to (`.marmel/prompts/<task_id>-validation-skip.md`)
/// for the one decision that may produce it: an explicit `(no-validate)` plan
/// directive ([`crate::plan_parse::ValidationDirective::NoValidate`]) on the
/// task's plan line. The record is the authoritative run-time signal for the read
/// side (`runner/execution.rs`, `agents/validation.rs`, `runner/fix_loop.rs`):
/// its absence keeps the fail-closed "no verdict file" guard intact, while its
/// presence means "the plan declared validation unnecessary, nothing to check".
pub(crate) const VALIDATION_SKIP_FILE_SUFFIX: &str = "-validation-skip.md";

/// One-line human-readable reason stored in the skip record.
///
/// It names the **plan line** as the author of the decision, not a keyword
/// match on the task description: the directive is what disabled validation.
pub(crate) const VALIDATION_SKIP_REASON: &str =
    "Plan directive (no-validate): validation not required";

/// Reason stored in the no-validation record of a task that **has no plan line**
/// (an ad-hoc task delegated by the Steer Arbitrator).
///
/// Deliberately a distinct constant from [`VALIDATION_SKIP_REASON`], and never a
/// rewording of it: that string asserts **plan provenance** ("the plan line said
/// `(no-validate)`"), which never existed for an ad-hoc task. Naming a plan
/// directive that no plan ever wrote would misreport the decision's origin on
/// disk and in the UI, and the plan wording is pinned verbatim by the lib test
/// `test_pregenerate_records_the_no_validation_decision` and by the
/// `tests/test_dynamic_prompts.rs` / `tests/test_validation_loop.rs` literals.
/// The ad-hoc record therefore says out loud what actually authorized it: the
/// delegation had no plan line at all, so the plan-side write never took a
/// validation decision for it.
pub(crate) const VALIDATION_SKIP_REASON_ADHOC: &str =
    "Ad-hoc delegated task (no plan line): validation not required";

/// The write-side outcome of one plan task line's validation directive.
///
/// Only [`Self::SkipRecord`] may ever create a `<task_id>-validation-skip.md`,
/// and it is reachable from exactly one directive value
/// ([`ValidationDirective::NoValidate`]). [`Self::Nothing`] exists for the roles
/// whose deliverables are never validated (`planner`, `validator`): they get
/// neither a brief nor a record, so their absent brief is never re-read as a
/// plan-designated skip.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WriteDecision {
    /// Write `<task_id>-validation.md` and drop any stale skip record.
    Brief,
    /// Write `<task_id>-validation-skip.md` and drop any stale brief.
    SkipRecord,
    /// Write neither record (role-exempt tasks).
    Nothing,
}

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
    /// File stem for this blueprint: the task id — an LLM-authored id from the
    /// `plan` checkbox or the delegation argument — checked by
    /// [`crate::task_id::validate_task_id`], the single owner of the task-id
    /// grammar. The id is normalized first (decoration stripping only, unchanged
    /// order) and the **normalized** value is what is validated and used, never
    /// sanitized, trimmed, clamped or mended into another name. A blueprint with
    /// **no task id** falls back to the crate-composed role name; an id empty
    /// after normalization yields [`crate::task_id::TaskIdError::Empty`].
    fn prompt_file_stem(&self) -> Result<&str, crate::task_id::TaskIdError> {
        match self.task_id.as_deref() {
            None => Ok(self.role_name.as_str()),
            Some(raw) => {
                let clean = crate::task_id::normalize_task_id_ref(raw);
                Ok(crate::task_id::validate_task_id(clean)?)
            }
        }
    }

    /// Save the synthesized prompt with YAML frontmatter to the given directory.
    ///
    /// Fail closed: a rejected task id produces
    /// [`std::io::ErrorKind::InvalidInput`] and **no file at all** — never a
    /// file written outside `dir` and never a file under a derived/mended name.
    /// Every caller already treats a failed save as "no prompt file on disk", so
    /// the downstream prompt and verdict lookups degrade exactly like a missing
    /// file (a hard verdict gap in the runner).
    pub fn save_to_disk(&self, dir: &Path) -> std::io::Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let file_stem = match self.prompt_file_stem() {
            Ok(stem) => stem,
            Err(err) => {
                tracing::warn!(
                    "Rejected task id {:?} while persisting the synthesized prompt for role {:?}: {err}. \
                     No prompt file is written and no substitute file name is derived, so prompt and \
                     verdict lookups for this task degrade exactly like a missing file.",
                    self.task_id,
                    self.role_name
                );
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    format!(
                        "task id {:?} rejected as prompt file stem: {err}",
                        self.task_id
                    ),
                ));
            }
        };

        let filename = format!("{file_stem}.md");
        // Defence in depth on top of the validator: the file name must be a
        // single path segment, so the joined destination stays a direct child of
        // `dir`. A name that would escape the prompts directory is refused — it
        // is never rewritten into a safe one.
        if std::path::Path::new(&filename).parent() != Some(std::path::Path::new("")) {
            tracing::warn!(
                "Refusing to persist a synthesized prompt as {filename:?}: the resolved file name is not a single path segment, so it would land outside {}.",
                dir.display()
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!(
                    "{filename:?} is not a single file name inside {}",
                    dir.display()
                ),
            ));
        }
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
            reasoning_effort: None,
            tools: None,
        };

        let mut raw_response = String::new();
        // Concurrency metering (design §5): the JIT blueprint is a **capped**
        // call, metered as `PromptArchitect` on purpose rather than inheriting
        // whatever class the caller installed. It happens before any worker is
        // registered and before the specialist it describes ever acquires a
        // slot, so it competes for the cap like any ordinary session and must
        // never borrow the steer/notice reserve. Identity is the delegation this
        // blueprint is written for, so a queued architect call names the task it
        // is blocking.
        let reply = pool::with_call_context(
            pool::CallContext::for_agent(
                pool::CallClass::PromptArchitect,
                req.agent_name.as_str(),
                req.task_id.clone(),
            ),
            client.chat_stream(&chat_req, |chunk| {
                raw_response.push_str(chunk);
                true
            }),
        )
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

        // Same rebirth guarantee as the offline synthesis: always present.
        let mut tools = parsed.allowed_tools;
        if !tools.iter().any(|t| t == TOOL_REBIRTH) {
            tools.push(TOOL_REBIRTH.to_string());
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
                    TOOL_READ_FILE.to_string(),
                    TOOL_GREP_SEARCH.to_string(),
                    TOOL_GLOB.to_string(),
                    TOOL_CREATE_PLAN.to_string(),
                    TOOL_REBIRTH.to_string(),
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

        if !allowed_tools.iter().any(|t| t == TOOL_REBIRTH) {
            allowed_tools.push(TOOL_REBIRTH.to_string());
        }

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
        system_prompt.push_str(&format!(
            "- **TERMINAL MARKERS:** Always conclude with `{MARKER_COMPLETE}` upon successful completion, or `{MARKER_FAILED}: <reason>` if impossible.\n"
        ));
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
    ///
    /// The task-line grammar is owned by [`crate::plan_parse`]; this is a thin
    /// mapping onto [`crate::plan_parse::parse_tasks`], so the prompt files
    /// generated from this list cover exactly the plan lines
    /// `Plan::pending_tasks` reports — bracketed, parenthesised, starred,
    /// numbered and bold-decorated task ids alike, and only unchecked boxes.
    /// The reserved validation directive (`(validate)` / `(no-validate)`) travels
    /// along verbatim as [`PlanTaskItem::validation_directive`]: this mapping adds
    /// no policy of its own, and a blank (`( )`) or unparseable (`(maybe-validate)`)
    /// paren group arrives as [`ValidationDirective::Auto`] exactly as the parser
    /// resolved it.
    pub fn parse_tasks_from_plan(markdown: &str) -> Vec<PlanTaskItem> {
        crate::plan_parse::parse_tasks(markdown)
            .into_iter()
            .filter(|task| task.is_pending())
            .filter_map(|task| {
                let description = task.description_without_role_hint().to_string();
                let role_hint = task.role_hint.clone();
                let validation_directive = task.validation_directive;
                let task_id = task.task_id?;
                Some(PlanTaskItem {
                    task_id,
                    description,
                    role_hint,
                    validation_directive,
                })
            })
            .collect()
    }

    /// File name of the `<task_id>-validation.md` prompt/verdict pair.
    ///
    /// File name of the `<task_id>-validation.md` prompt/verdict pair: the id
    /// must pass [`crate::task_id::validate_task_id`] before it is joined onto
    /// the prompts directory, because this name is written here and read back by
    /// the runner verdict path and by `agents::validation`. The plan-line grammar
    /// strips id decoration ([`crate::plan_parse`] via
    /// [`crate::task_id::normalize_task_id_ref`]); that normalization is
    /// deliberately **not** folded in, so the value validated is the value used.
    /// A rejection is a [`crate::task_id::TaskIdError`], never a repaired name.
    fn validation_prompt_file_name(
        raw_task_id: &str,
    ) -> Result<String, crate::task_id::TaskIdError> {
        let id = crate::task_id::validate_task_id(raw_task_id)?;
        Ok(format!("{id}-validation.md"))
    }

    /// File name of the `<task_id>-validation-skip.md` record.
    ///
    /// The record is the durable spelling of the write-side decision taken from
    /// the plan line's explicit `(no-validate)` directive
    /// ([`ValidationDirective::NoValidate`]): it says out loud that the plan
    /// declared this task validation-free, so the read side never mistakes the
    /// absent `<task_id>-validation.md` for a lost verdict file. Same grammar gate
    /// as [`Self::validation_prompt_file_name`] — a rejected id is a
    /// [`crate::task_id::TaskIdError`], never a repaired name, and a rejected id
    /// therefore never gets a record at all.
    fn validation_skip_file_name(raw_task_id: &str) -> Result<String, crate::task_id::TaskIdError> {
        let id = crate::task_id::validate_task_id(raw_task_id)?;
        Ok(format!("{id}{VALIDATION_SKIP_FILE_SUFFIX}"))
    }

    /// The single owner of the write-side precedence: plan directive → outcome.
    ///
    /// * [`ValidationDirective::NoValidate`] (`(no-validate)`) is the **only**
    ///   input that can ever yield [`WriteDecision::SkipRecord`].
    /// * [`ValidationDirective::Validate`] (`(validate)`) forces the brief.
    /// * [`ValidationDirective::Auto`] (no reserved token on the line) writes the
    ///   brief too — the plan says nothing, so validation stays on. It is never
    ///   allowed to produce a skip record.
    ///
    /// Fail closed on wording: a blank (`( )`) or unparseable (`(maybe-validate)`)
    /// parenthetical is already `Auto` because [`ValidationDirective::parse`] maps
    /// every token outside [`crate::plan_parse::VALIDATION_DIRECTIVES`] to `Auto`.
    /// The assertion below re-checks that guarantee **on this side** so the write
    /// path can never fall through to `NoValidate` — not today, and not if the
    /// parser's default ever moves.
    fn validation_write_decision(
        directive: ValidationDirective,
        agent: crate::agents::Agent,
    ) -> WriteDecision {
        debug_assert!(
            ValidationDirective::parse("").is_auto()
                && ValidationDirective::parse("   ").is_auto()
                && ValidationDirective::parse("( )").is_auto()
                && ValidationDirective::parse("maybe-validate").is_auto(),
            "a blank or unparseable validation directive must resolve to Auto, never to NoValidate"
        );
        // `planner` / `validator` deliverables are exempt from validation by role
        // (the read side hard-codes that opt-out), so no brief is authored for
        // them — an explicit `(validate)` cannot buy back a role exemption. An
        // explicit `(no-validate)` is still recorded, because that is what the
        // plan line says.
        let role_exempt = matches!(
            agent,
            crate::agents::Agent::Planner | crate::agents::Agent::Validator
        );
        match directive {
            ValidationDirective::NoValidate => WriteDecision::SkipRecord,
            ValidationDirective::Validate | ValidationDirective::Auto if role_exempt => {
                WriteDecision::Nothing
            }
            ValidationDirective::Validate | ValidationDirective::Auto => WriteDecision::Brief,
        }
    }

    /// Remove a stale `<task_id>-validation-skip.md` left over from an earlier plan.
    ///
    /// Best effort: the outcome is reported through tracing, never returned, because
    /// the caller is about to write the contradicting brief and the read side's
    /// fail-closed guard still holds if the record survives.
    fn delete_stale_skip_record(prompts_dir: &Path, raw_task_id: &str) {
        let Ok(skip_name) = Self::validation_skip_file_name(raw_task_id) else {
            return;
        };
        let skip_path = prompts_dir.join(&skip_name);
        if skip_path.exists()
            && let Err(err) = std::fs::remove_file(&skip_path)
        {
            tracing::error!(
                "Failed to delete the stale no-validation record {} for task {:?}: {err}. \
                 Its validation brief is written next, and if that write fails this task \
                 keeps a contradictory pair of records on disk.",
                skip_path.display(),
                raw_task_id
            );
        }
    }

    /// Remove a stale `<task_id>-validation.md` left over from an earlier plan.
    ///
    /// `Ok(true)` when a brief really was there and is gone, `Ok(false)` when there
    /// was nothing to remove (including a grammar-rejected task id, which never had
    /// a brief name authored here). `Err` only when an existing brief could not be
    /// deleted — the caller must then **not** write a skip record, so the two
    /// records can never contradict each other and the task stays fail closed
    /// against the brief that is still on disk.
    fn delete_stale_validation_brief(
        prompts_dir: &Path,
        raw_task_id: &str,
    ) -> Result<bool, std::io::Error> {
        let Ok(name) = Self::validation_prompt_file_name(raw_task_id) else {
            return Ok(false);
        };
        match std::fs::remove_file(prompts_dir.join(name)) {
            Ok(()) => Ok(true),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(err) => Err(err),
        }
    }

    /// Write the durable "this task needs no validation" record and return its
    /// file name, or `None` when the task id is refused by the grammar authority
    /// (warned about, never repaired into some other name).
    ///
    /// This is the **single** writer of `<task_id>-validation-skip.md`, so the
    /// grammar gate ([`Self::validation_skip_file_name`]), the one-line file shape
    /// and the failure log live here once. `reason` names the **provenance** of the
    /// decision and is the only difference between the two records that can exist:
    /// [`VALIDATION_SKIP_REASON`] for a plan line carrying `(no-validate)`,
    /// [`VALIDATION_SKIP_REASON_ADHOC`] for a delegated task with no plan line.
    /// The read side treats the string as opaque (only a non-blank record counts),
    /// so it must stay exactly one line + `\n` — a blank record reads as "no
    /// decision" and would silently drop the exemption.
    pub(crate) fn write_validation_skip_record(
        prompts_dir: &Path,
        task_id: &str,
        reason: &str,
    ) -> Option<String> {
        let name = match Self::validation_skip_file_name(task_id) {
            Ok(name) => name,
            Err(err) => {
                tracing::warn!(
                    "Rejected task id {task_id:?} while recording its no-validation decision under {}: {err}. \
                     No `-validation-skip.md` record is written and no substitute name is derived.",
                    prompts_dir.display()
                );
                return None;
            }
        };
        match std::fs::write(prompts_dir.join(&name), format!("{reason}\n")) {
            Ok(()) => Some(name),
            Err(err) => {
                tracing::warn!(
                    "Failed to record the no-validation decision for task {task_id:?} under {}: {err}. \
                     The task keeps today's fail-closed verdict behaviour.",
                    prompts_dir.display()
                );
                None
            }
        }
    }

    /// Record that an **ad-hoc delegated task** — one with no plan line — needs no
    /// validation, and return the record's file name (`None` when no record may be
    /// or was authored).
    ///
    /// This is the write-side counterpart of [`Self::validation_write_decision`] for
    /// tasks the plan never produced. The ad-hoc steering path
    /// ([`crate::orchestrator::steer::execute_steer_subtask`]) mints a valid task id
    /// and delegates without ever touching `create_plan`, so no write-side decision
    /// was ever taken for it and the read side sees a verdict gap
    /// (`<task_id>-validation.md` missing, no record) and fails the deliverable
    /// closed. Authoring this record before the run is what turns that default into
    /// "no validation", and it is the only producer of
    /// [`VALIDATION_SKIP_REASON_ADHOC`].
    ///
    /// Every step below is load-bearing:
    /// 1. **Grammar gate** — the same check as [`Self::validation_skip_file_name`] /
    ///    [`AgentBlueprint::save_to_disk`]. An id that cannot be safely addressed
    ///    gets no record and no repaired name, so it keeps the fail-closed gap.
    /// 2. **Role opt-out** — `planner` / `validator` deliverables are never
    ///    validation-eligible on the read side, so a record for them would be inert
    ///    and would contradict the documented role-level opt-out.
    /// 3. **Plan-collision guard** — an ad-hoc id that *is* a line of the active
    ///    plan keeps **plan** semantics: it must not be exempted by an ad-hoc
    ///    default, and its authored brief must not be destroyed by step 4. This is
    ///    what stops an unrelated ad-hoc deliverable from checking off (or silently
    ///    de-validating) a planned task.
    /// 4. **Stale-brief hygiene** — identical to the plan path's `SkipRecord` arm:
    ///    a readable brief ranks above a record on the read side, so a surviving
    ///    brief would make the record a lie. On delete failure **no record is
    ///    authored** and `None` is returned, which keeps the fail-closed behaviour
    ///    rather than manufacturing an exemption.
    /// 5. Write through [`Self::write_validation_skip_record`], the single owner of
    ///    the record shape. A failed write is reported there and returns `None` —
    ///    never a silent success.
    ///
    /// `pub` (like [`Self::pregenerate_for_plan_offline`]) so the integration tests
    /// under `tests/` drive the real writer rather than hand-written fixtures.
    pub fn record_adhoc_no_validation(
        prompts_dir: &Path,
        raw_task_id: &str,
        agent: crate::agents::Agent,
        plan_markdown: Option<&str>,
    ) -> Option<String> {
        // 1. Grammar authority first: never mint an exemption for an id that cannot
        //    be safely addressed as a single path segment.
        let normalized = crate::task_id::normalize_task_id_ref(raw_task_id);
        let id = match crate::task_id::validate_task_id(normalized) {
            Ok(id) => id,
            Err(err) => {
                tracing::warn!(
                    "Rejected task id {raw_task_id:?} while recording its ad-hoc no-validation default under {}: {err}. \
                     No `-validation-skip.md` record is written and no substitute name is derived, so this task \
                     keeps the fail-closed verdict guard.",
                    prompts_dir.display()
                );
                return None;
            }
        };

        // 2. Role-level opt-outs are never briefed and never recorded: the read side
        //    does not probe for these roles at all.
        if matches!(
            agent,
            crate::agents::Agent::Planner | crate::agents::Agent::Validator
        ) {
            return None;
        }

        // 3. An id that belongs to the plan is a **planned** task, whatever the
        //    steering path called it. Plan semantics stay untouched.
        if plan_markdown.is_some_and(|md| crate::plan_parse::find_task_line(md, id).is_some()) {
            tracing::warn!(
                "Steering asked for an ad-hoc no-validation default for task {id:?}, but that id is a line of the \
                 active execution plan. No ad-hoc record is written: the plan's own validation decision (brief or \
                 `(no-validate)` record) stays authoritative."
            );
            return None;
        }

        // 4. Mutual exclusivity with the brief, exactly as the plan path does it.
        match Self::delete_stale_validation_brief(prompts_dir, id) {
            Ok(true) => tracing::info!(
                "Deleted a validation brief left for task {id:?} by an earlier plan before recording its \
                 ad-hoc no-validation default."
            ),
            Ok(false) => {}
            Err(err) => {
                tracing::error!(
                    "Failed to delete the stale validation brief of task {id:?} under {dir}: {err}. \
                     No `-validation-skip.md` record is written, so this task keeps the fail-closed verdict \
                     guard against the brief that is still on disk.",
                    dir = prompts_dir.display()
                );
                return None;
            }
        }

        // 5. One writer, one line, one reason.
        let _ = std::fs::create_dir_all(prompts_dir);
        Self::write_validation_skip_record(prompts_dir, id, VALIDATION_SKIP_REASON_ADHOC)
    }

    /// Read the durable no-validation record for `raw_task_id` inside `prompts_dir`.
    ///
    /// `Some(reason)` only when a non-blank record exists. A rejected id, a
    /// missing record and a blank record all read as `None`, so the fail-closed
    /// verdict guard is never weakened by a corrupt or absent record.
    pub(crate) fn validation_skip_record(prompts_dir: &Path, raw_task_id: &str) -> Option<String> {
        let name = Self::validation_skip_file_name(raw_task_id).ok()?;
        let text = std::fs::read_to_string(prompts_dir.join(name)).ok()?;
        let reason = text.trim();
        (!reason.is_empty()).then(|| reason.to_string())
    }

    /// Convenience lookup for read paths that do not already hold the prompts
    /// directory: resolves `<workspace root>/.marmel/prompts` and normalizes the
    /// task id exactly as the `{tid}-validation.md` verdict lookup does.
    pub(crate) fn validation_skip_record_for_task(task_id: Option<&str>) -> Option<String> {
        let raw = task_id?;
        let prompts_dir = crate::harness::get_workspace_root()
            .join(crate::manager::phase::MARMEL_DIR)
            .join("prompts");
        let normalized = crate::task_id::normalize_task_id_ref(raw);
        Self::validation_skip_record(&prompts_dir, normalized)
    }

    /// Pre-generate prompt pairs (`<task_id>.md` and, unless the plan line says
    /// `(no-validate)`, `<task_id>-validation.md`) for all tasks in a plan.
    ///
    /// The outcome per task comes from [`PlanTaskItem::validation_directive`]
    /// through [`Self::validation_write_decision`], and each write also clears the
    /// contradicting record from any earlier plan: `(validate)`/`Auto` delete a
    /// stale `<task_id>-validation-skip.md` **before** the brief is attempted,
    /// `(no-validate)` deletes a stale `<task_id>-validation.md` before its record
    /// is written. A task id refused by the grammar authority contributes neither
    /// record.
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

            let worker_bp = Self::synthesize_offline(catalog, &req);
            if let Ok(path) = worker_bp.save_to_disk(prompts_dir) {
                generated.push(
                    path.file_name()
                        .unwrap_or_default()
                        .to_string_lossy()
                        .to_string(),
                );
            }

            // The **plan line** decides, never its wording: `(no-validate)` is the
            // only spelling that may produce a `-validation-skip.md` record,
            // `(validate)` forces the brief, and a line carrying no reserved token
            // (`Auto`) keeps validation on. The directive is parsed by
            // [`crate::plan_parse`] and carried here on
            // [`PlanTaskItem::validation_directive`]; a blank (`( )`) or
            // unparseable (`(maybe-validate)`) paren group is `Auto` and can never
            // reach the skip arm.
            let decision = Self::validation_write_decision(item.validation_directive, agent);

            match decision {
                // `planner` / `validator` deliverables are exempt from validation by
                // role on the read side, so neither record is authored for them.
                WriteDecision::Nothing => {}
                WriteDecision::SkipRecord => {
                    // Stale-record lifecycle on re-plan, flip-to-skipped direction:
                    // a task that was validated under an earlier plan and is re-planned
                    // with `(no-validate)` must not keep the previous plan's
                    // `<tid>-validation.md`, because the read side ranks a readable
                    // brief above the record and would audit the deliverable against a
                    // **stale** brief. The brief is therefore deleted first, together
                    // with the flipped decision.
                    match Self::delete_stale_validation_brief(prompts_dir, &item.task_id) {
                        Ok(true) => tracing::info!(
                            "Deleted the validation brief an earlier plan left for task {:?}: \
                             its plan line now carries `(no-validate)`.",
                            item.task_id
                        ),
                        Ok(false) => {}
                        Err(err) => {
                            // Half-applied flips are never allowed to contradict
                            // themselves: while a brief is still readable on disk the
                            // skip record would be ignored anyway, so recording a
                            // plan-designated skip on top of it would misreport what the
                            // read side does. The task keeps its fail-closed handling.
                            tracing::error!(
                                "Failed to delete the stale validation brief of task {:?} under {}: {err}. \
                                 No `-validation-skip.md` record is written for this task.",
                                item.task_id,
                                prompts_dir.display()
                            );
                            continue;
                        }
                    }
                    // The absence of a validation brief is a **decision made by the plan
                    // line**, so it is recorded durably rather than left for the read
                    // side to infer. A task with neither record — e.g. a task id refused
                    // by the grammar authority below — keeps the fail-closed verdict
                    // guard instead of a manufactured exemption.
                    if let Some(skip_name) = Self::write_validation_skip_record(
                        prompts_dir,
                        &item.task_id,
                        VALIDATION_SKIP_REASON,
                    ) {
                        generated.push(skip_name);
                    }
                }
                WriteDecision::Brief => {
                    // The `<task_id>-validation.md` name is a WRITE whose file stem
                    // is an LLM-authored plan id, so it is checked by
                    // [`Self::validation_prompt_file_name`] before any join. A
                    // rejected id writes nothing and never gets a substitute name;
                    // the runner verdict path and `agents::validation` then see
                    // exactly what they see for an absent validation prompt.
                    let val_filename = match Self::validation_prompt_file_name(&item.task_id) {
                        Ok(name) => name,
                        Err(err) => {
                            tracing::warn!(
                                "Rejected task id {:?} while pre-generating its validation prompt under {}: {err}. \
                             No `-validation.md` file is written and no substitute name is derived.",
                                item.task_id,
                                prompts_dir.display()
                            );
                            continue;
                        }
                    };
                    let val_dest = prompts_dir.join(&val_filename);
                    let val_content = format!(
                        "---\nrole_name: \"validator\"\ntask_id: \"{}\"\ntarget_brief: \"{}\"\ncreated_at: \"{}\"\nallowed_tools:\n  - \"read_file\"\n  - \"grep_search\"\n  - \"glob\"\n  - \"run_command\"\n  - \"leave_verdict\"\n  - \"rebirth\"\n---\n\n# Independent Quality Verification for {}\n\n**Mission:** Independently audit the deliverable for task `{}`.\n**Brief:** {}\n\n## Verification Protocol:\n1. Inspect created or modified files to verify correctness and conformance. Keep inspection targeted and concise (aim for 2–5 inspection tool calls; avoid exhaustive codebase exploration).\n2. Execute test suites or checks if applicable.\n3. Conclude promptly by calling `leave_verdict(verdict=\"APPROVED\" | \"REJECTED\", comments=\"...\")`.\n",
                        item.task_id,
                        item.description.replace('"', "\\\""),
                        chrono::Utc::now().to_rfc3339(),
                        item.task_id,
                        item.task_id,
                        item.description
                    );
                    // The two records are written as one atomic **intent**: a task that
                    // is meant to be validated must never keep a no-validation record,
                    // and it must not keep one when the brief cannot be written either.
                    // A validation prompt now exists (or is about to be attempted) for
                    // this task, so any stale `{tid}-validation-skip.md` from an earlier
                    // plan is dropped **first** — deleting it up front means a failed
                    // `-validation.md` write below can never leave the task looking
                    // "skipped by design".
                    Self::delete_stale_skip_record(prompts_dir, &item.task_id);
                    if let Err(err) = std::fs::write(&val_dest, &val_content) {
                        // Fail closed, never fail open: the write of the validation brief
                        // failed (IO error, read-only directory, rejected destination), so
                        // this task has **no** brief and — by the deletion above — **no**
                        // skip record either. The read side therefore keeps the fail-closed
                        // verdict gap instead of silently reporting the task as
                        // "validation skipped by design" and checking off its plan line.
                        // The failure is reported like every other prompt-write failure in
                        // this file, so it is never swallowed.
                        tracing::warn!(
                            "Failed to write the validation prompt {} for task {:?}: {err}. \
                         No `-validation-skip.md` record is left behind for this task, so its missing \
                         `-validation.md` stays a fail-closed verdict gap and is never re-read as a \
                         plan-designated skip.",
                            val_dest.display(),
                            item.task_id
                        );
                    } else {
                        generated.push(val_filename);
                    }
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
    /// The reserved validation directive of the plan line, copied verbatim from
    /// [`crate::plan_parse::TaskLine::validation_directive`]: `(validate)` /
    /// `(no-validate)`, or [`ValidationDirective::Auto`] when the line carries no
    /// reserved token — including a blank (`( )`) or unparseable
    /// (`(maybe-validate)`) parenthetical, which the parser resolves to `Auto`
    /// and this mapping never upgrades into `NoValidate`. It is what decides
    /// whether a validation brief or a no-validation record is written; the
    /// description wording no longer decides anything.
    pub validation_directive: ValidationDirective,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::Agent;
    use crate::tool_names::{TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE};

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
        assert!(
            blueprint
                .allowed_tools
                .contains(&TOOL_RUN_COMMAND.to_string())
        );
        assert!(blueprint.allowed_tools.contains(&TOOL_REPLACE.to_string()));
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
                TOOL_READ_FILE.to_string(),
                TOOL_WRITE_FILE.to_string(),
                TOOL_REBIRTH.to_string(),
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
            vec![TOOL_READ_FILE, TOOL_WRITE_FILE, TOOL_REBIRTH]
        );
        assert_eq!(loaded.system_prompt, "You are a test agent.");
    }

    /// The plan **directive** decides which prompt files exist, not the wording of
    /// the task line: `(no-validate)` records the skip and writes no brief,
    /// `(validate)` writes the brief, and a line carrying no reserved token
    /// (`Auto`) writes the brief too — even when its wording reads like research or
    /// documentation, because task wording never gates validation on its own.
    #[test]
    fn test_pregenerate_for_plan_offline() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let plan = r#"
# Execution Plan

### Phase 1: Research
- [ ] [t-001] Investigate API contracts (no-validate) (researcher)

### Phase 2: Implementation
- [ ] [t-002] Implement parser logic (validate) (coder)
- [ ] [t-004] Migrate the schema index (coder)

### Phase 3: Documentation
- [ ] [t-003] Produce documentation in README.md (generalist)
"#;
        let generated = PromptBuilder::pregenerate_for_plan_offline(plan, &catalog, tmp.path());
        assert!(generated.contains(&"t-001.md".to_string()));
        assert!(generated.contains(&"t-002.md".to_string()));
        assert!(generated.contains(&"t-003.md".to_string()));
        assert!(generated.contains(&"t-004.md".to_string()));
        // `(validate)` on the coder task: the brief, and never a skip record.
        assert!(generated.contains(&"t-002-validation.md".to_string()));
        assert!(!generated.contains(&"t-002-validation-skip.md".to_string()));
        // `(no-validate)` on the researcher task: no brief at all.
        assert!(!generated.contains(&"t-001-validation.md".to_string()));
        assert!(generated.contains(&"t-001-validation-skip.md".to_string()));
        // `Auto` lines are validated whatever their wording says, so both the
        // keyword-free coder line and the README-documentation line get a brief and
        // neither carries a record.
        assert!(generated.contains(&"t-004-validation.md".to_string()));
        assert!(generated.contains(&"t-003-validation.md".to_string()));
        assert!(!generated.contains(&"t-003-validation-skip.md".to_string()));
        assert!(!generated.contains(&"t-004-validation-skip.md".to_string()));
    }

    /// The decision not to write a validation prompt comes from the plan line and is
    /// recorded durably, so the read side can tell "the plan declared this
    /// validation-free" apart from a lost verdict file. A `(no-validate)` line gets
    /// `<tid>-validation-skip.md` with the one-line reason naming the **directive**
    /// as the author — including on a coder task, where neither role nor wording
    /// buys validation back — while `(validate)` and `Auto` get the brief and
    /// **no** record.
    #[test]
    fn test_pregenerate_records_the_no_validation_decision() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        let plan = "\
# Execution Plan
- [ ] [t-001] Investigate API contracts (no-validate) (researcher)
- [ ] [t-002] Implement parser logic (coder)
- [ ] [t-003] Produce documentation in README.md (no-validate) (generalist)
- [ ] [t-004] Fix the parser crash (no-validate) (coder)
";

        let generated = PromptBuilder::pregenerate_for_plan_offline(plan, &catalog, &prompts);

        assert!(generated.contains(&"t-001-validation-skip.md".to_string()));
        assert!(generated.contains(&"t-003-validation-skip.md".to_string()));
        // `(no-validate)` on a **coder** task: the directive outranks the role, and
        // neither the role nor the description wording can buy validation back.
        assert!(generated.contains(&"t-004-validation-skip.md".to_string()));
        assert!(!prompts.join("t-004-validation.md").exists());
        // An `Auto` coder line is validated and contributes no record.
        assert!(!generated.contains(&"t-002-validation-skip.md".to_string()));
        assert_eq!(
            std::fs::read_to_string(prompts.join("t-001-validation-skip.md"))
                .unwrap()
                .trim(),
            crate::agents::prompt_builder::VALIDATION_SKIP_REASON,
            "the skip record is a one-line reason naming the plan directive"
        );
        assert!(
            crate::agents::prompt_builder::VALIDATION_SKIP_REASON.contains("Plan directive"),
            "the reason must credit the plan line, not a keyword match on the description"
        );
        assert!(!prompts.join("t-002-validation-skip.md").exists());
        // A validation prompt and a skip record never coexist for one task.
        assert!(prompts.join("t-002-validation.md").exists());
    }

    /// Regression pin (t-014b): the reserved plan-line directive never reaches
    /// model-facing prompt text. Both authored files of a directive-bearing task —
    /// the `<tid>.md` blueprint and the `<tid>-validation.md` whose `target_brief`
    /// is the task brief — are free of the `(validate)` / `(no-validate)`
    /// vocabulary, while still naming the task id, the role and the plan wording.
    /// The directive stays honoured where it belongs: as the write-side decision
    /// (skip record for `(no-validate)`, brief for `(validate)` and for a line with
    /// no token at all).
    #[test]
    fn test_generated_prompts_never_carry_the_reserved_directive() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        let plan = "\
# Execution Plan
- [ ] [t-070] Investigate API contracts (no-validate) (researcher)
- [ ] [t-071] Implement the parser core (validate) (coder)
- [ ] [t-072] Refactor the schema index (coder)
";
        // The parsed description is already directive-free, so nothing downstream
        // (blueprint prompt, target brief) can re-spell the token.
        let items = PromptBuilder::parse_tasks_from_plan(plan);
        assert_eq!(items[0].description, "Investigate API contracts");
        assert_eq!(items[1].description, "Implement the parser core");

        let generated = PromptBuilder::pregenerate_for_plan_offline(plan, &catalog, &prompts);

        // 1. The directive still decides.
        assert!(generated.contains(&"t-070-validation-skip.md".to_string()));
        assert!(
            !prompts.join("t-070-validation.md").exists(),
            "a `(no-validate)` task must not get a validation brief"
        );
        assert!(generated.contains(&"t-071-validation.md".to_string()));
        assert!(generated.contains(&"t-072-validation.md".to_string()));

        // 2. No authored **prompt** file spells the reserved vocabulary. (The
        //    `<tid>-validation-skip.md` record is a provenance record naming the
        //    plan directive by design, and is deliberately not a prompt.)
        for name in [
            "t-070.md",
            "t-071.md",
            "t-072.md",
            "t-071-validation.md",
            "t-072-validation.md",
        ] {
            let text = std::fs::read_to_string(prompts.join(name))
                .unwrap_or_else(|err| panic!("{name} must exist: {err}"));
            let lowered = text.to_ascii_lowercase();
            assert!(
                !lowered.contains("(no-validate)"),
                "{name} leaked the reserved directive:\n{text}"
            );
            assert!(
                !lowered.contains("(validate)"),
                "{name} leaked the reserved directive:\n{text}"
            );
        }

        // 3. The stripped text still identifies the right task and role.
        let blueprint =
            std::fs::read_to_string(prompts.join("t-070.md")).expect("t-070.md must exist");
        assert!(blueprint.contains("task_id: \"t-070\""), "{blueprint}");
        assert!(
            blueprint.contains("role_name: \"researcher_specialist\""),
            "the role hint must still route the task:\n{blueprint}"
        );
        let brief = std::fs::read_to_string(prompts.join("t-071-validation.md"))
            .expect("t-071-validation.md must exist");
        assert!(brief.contains("task_id: \"t-071\""), "{brief}");
        assert!(
            brief.contains("target_brief: \"Implement the parser core\""),
            "the target brief must be the directive-free plan wording:\n{brief}"
        );
        assert!(
            brief.contains("**Brief:** Implement the parser core"),
            "the validator mission must carry the directive-free wording:\n{brief}"
        );
    }

    /// Re-plan flip **no-validate → validate**: the task that carried a
    /// `-validation-skip.md` record gets its validation brief and the stale record
    /// is dropped, so the two records cannot disagree on disk. The read-side lookup
    /// follows the record (present / absent / blank), never the wording.
    #[test]
    fn test_validation_skip_record_lookup_and_stale_cleanup() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        // First plan: the plan line declares `(no-validate)` → skip record, no brief.
        PromptBuilder::pregenerate_for_plan_offline(
            "- [ ] [t-056] Write the release notes (no-validate) (generalist)\n",
            &catalog,
            &prompts,
        );
        assert!(prompts.join("t-056-validation-skip.md").exists());
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "t-056").as_deref(),
            Some(crate::agents::prompt_builder::VALIDATION_SKIP_REASON)
        );

        // Re-planned with `(validate)`: the validation prompt wins and the stale
        // record is dropped.
        PromptBuilder::pregenerate_for_plan_offline(
            "- [ ] [t-056] Implement the widget factory (validate) (coder)\n",
            &catalog,
            &prompts,
        );
        assert!(prompts.join("t-056-validation.md").exists());
        assert!(
            !prompts.join("t-056-validation-skip.md").exists(),
            "a stale skip record must not outlive the validation prompt"
        );
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "t-056"),
            None
        );

        // No record at all, and a blank record, both read as "no decision".
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "t-057"),
            None
        );
        std::fs::write(prompts.join("t-057-validation-skip.md"), " \n").expect("blank skip record");
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "t-057"),
            None
        );
        // A hostile id is refused by the grammar authority, never repaired.
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "../x"),
            None
        );
    }

    /// Re-plan flip **validate → no-validate**: when the new plan line declares
    /// `(no-validate)`, the `<tid>-validation.md` authored by the previous plan MUST
    /// be deleted. The read side ranks a readable brief above the record, so leaving
    /// that file behind makes the next run audit the deliverable against a **stale**
    /// brief from another plan instead of honouring the directive.
    #[test]
    fn test_replan_to_no_validate_drops_the_stale_validation_brief() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        // First plan: `(validate)` → brief on disk, no record.
        PromptBuilder::pregenerate_for_plan_offline(
            "- [ ] [t-058] Implement the widget factory (validate) (coder)\n",
            &catalog,
            &prompts,
        );
        assert!(prompts.join("t-058-validation.md").exists());
        assert!(!prompts.join("t-058-validation-skip.md").exists());

        // Re-planned with `(no-validate)`: the record is written AND the previous
        // plan's brief is deleted.
        PromptBuilder::pregenerate_for_plan_offline(
            "- [ ] [t-058] Summarize the findings for the release (no-validate) (researcher)\n",
            &catalog,
            &prompts,
        );
        assert!(
            !prompts.join("t-058-validation.md").exists(),
            "a stale validation brief must not outlive the `(no-validate)` directive"
        );
        assert!(prompts.join("t-058-validation-skip.md").exists());
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "t-058").as_deref(),
            Some(crate::agents::prompt_builder::VALIDATION_SKIP_REASON)
        );
        // The read side's own probe now sees a genuinely absent brief — the only
        // state a skip record is allowed to explain.
        assert!(
            crate::agents::validation::probe_validation_brief(&prompts, "t-058").is_absent(),
            "the re-planned task must not be audited against a brief from an earlier plan"
        );
    }

    /// An `Auto` plan line (no reserved token) writes its brief and **no** record,
    /// so a re-plan can never leave the task with a stale record or with a missing
    /// brief — the fail-closed verdict gap and the plan-designated exemption are both
    /// reserved for lines that say something explicit.
    #[test]
    fn test_auto_directive_yields_neither_a_stale_record_nor_a_missing_brief() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        for plan in [
            "- [ ] [t-059] Implement the widget factory (coder)\n",
            "- [ ] [t-059] Write the release notes (generalist)\n",
        ] {
            PromptBuilder::pregenerate_for_plan_offline(plan, &catalog, &prompts);
            assert!(
                prompts.join("t-059-validation.md").exists(),
                "an Auto line must always have its brief: {plan}"
            );
            assert!(
                !prompts.join("t-059-validation-skip.md").exists(),
                "Auto must never produce a skip record: {plan}"
            );
            assert_eq!(
                PromptBuilder::validation_skip_record(&prompts, "t-059"),
                None
            );
        }
    }

    /// A `(no-validate)` line never fabricates a record for a task id the grammar
    /// authority refuses: no record name is returned, no file is created, and no
    /// substitute name is derived.
    #[test]
    fn test_no_validate_writes_no_record_for_a_rejected_task_id() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        let long = format!("t-{}", "z".repeat(70));
        let plan = format!("- [ ] [{long}] Fix the parser crash (no-validate) (coder)\n");

        let generated = PromptBuilder::pregenerate_for_plan_offline(&plan, &catalog, &prompts);

        assert!(
            !generated.iter().any(|name| name.contains(&long)),
            "a rejected plan id must contribute no file name: {generated:?}"
        );
        let strays: Vec<String> = std::fs::read_dir(&prompts)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| name.contains("validation"))
            .collect();
        assert!(
            strays.is_empty(),
            "a rejected id must never get a validation record of any kind: {strays:?}"
        );
    }

    /// The two records are written with one atomic **intent**: a task that is meant
    /// to be validated must never keep a no-validation record — not even when the
    /// `-validation.md` write itself fails (IO error, read-only directory, rejected
    /// destination). The stale record is dropped **before** the brief is attempted,
    /// so the task ends with neither record and the read side keeps its fail-closed
    /// verdict gap instead of reporting the task as "validation skipped by design".
    /// The fixture blocks the brief destination with a directory, which fails the
    /// write while leaving the prompts directory itself writable.
    #[test]
    fn test_failed_validation_brief_write_leaves_no_skip_record_and_warns() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        // Round 1: the plan line declares `(no-validate)` → the record is written.
        PromptBuilder::pregenerate_for_plan_offline(
            "- [ ] [t-060] Produce the release notes (no-validate) (generalist)\n",
            &catalog,
            &prompts,
        );
        assert!(prompts.join("t-060-validation-skip.md").exists());

        // Round 2: the same id is re-planned with `(validate)`, but its
        // validation brief cannot be written.
        std::fs::create_dir(prompts.join("t-060-validation.md")).expect("blocking dir");
        let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let _tracing_lock = crate::orchestrator::workers::TEST_TRACING_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let generated = tracing::subscriber::with_default(
            PromptWriteWarns(counter.clone(), "Failed to write the validation prompt"),
            || {
                tracing::callsite::rebuild_interest_cache();
                PromptBuilder::pregenerate_for_plan_offline(
                    "- [ ] [t-060] Implement the widget factory (validate) (coder)\n",
                    &catalog,
                    &prompts,
                )
            },
        );
        drop(_tracing_lock);

        assert!(
            !generated.contains(&"t-060-validation.md".to_string()),
            "a brief that could not be written must not be reported as generated: {generated:?}"
        );
        assert!(
            !generated.contains(&"t-060-validation-skip.md".to_string()),
            "a failed brief write must never (re-)record a skip decision: {generated:?}"
        );
        assert!(
            !prompts.join("t-060-validation-skip.md").exists(),
            "a stale skip record must never outlive a failed validation-brief write"
        );
        // The read side's authoritative lookup now finds no decision at all, so the
        // task is NOT exempted from the verdict check: it stays fail closed.
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "t-060"),
            None,
            "no surviving skip record may rescue the task after a failed brief write"
        );
        assert!(
            counter.load(std::sync::atomic::Ordering::Relaxed) >= 1,
            "the failed validation-brief write must be reported, never swallowed"
        );
    }

    /// The **directive** is the single owner of the write-side outcome; the
    /// description wording decides nothing. `(no-validate)` on a coder task records
    /// the skip, `(validate)` on a researcher task writes the brief, and `Auto`
    /// writes the brief without ever recording a skip. Blank and unparseable
    /// directive values resolve to `Auto` — asserted here on the write side as well,
    /// so nothing can fall through to `NoValidate`.
    #[test]
    fn test_validation_directive_is_the_owner_not_the_wording() {
        // The mapping itself: `NoValidate` is the only route to a skip record.
        assert_eq!(
            PromptBuilder::validation_write_decision(ValidationDirective::NoValidate, Agent::Coder),
            WriteDecision::SkipRecord
        );
        assert_eq!(
            PromptBuilder::validation_write_decision(ValidationDirective::Validate, Agent::Coder),
            WriteDecision::Brief
        );
        assert_eq!(
            PromptBuilder::validation_write_decision(ValidationDirective::Auto, Agent::Coder),
            WriteDecision::Brief
        );
        // A blank or unparseable directive value can only ever be `Auto` on this
        // side too — never `NoValidate`, and therefore never a skip record.
        for blank_or_junk in [
            "",
            " ",
            "  \t ",
            "( )",
            "maybe-validate",
            "no validate",
            "auto",
            "validate?",
            "skip validation",
        ] {
            let parsed = ValidationDirective::parse(blank_or_junk);
            assert!(
                parsed.is_auto(),
                "{blank_or_junk:?} must resolve to Auto, got {parsed:?}"
            );
            assert_eq!(
                PromptBuilder::validation_write_decision(parsed, Agent::Coder),
                WriteDecision::Brief,
                "{blank_or_junk:?} must never reach the skip-record arm"
            );
        }
        // Only the exact reserved token flips the outcome.
        assert_eq!(
            PromptBuilder::validation_write_decision(
                ValidationDirective::parse("NO-VALIDATE"),
                Agent::Researcher
            ),
            WriteDecision::SkipRecord
        );

        // On disk, the same precedence: role and wording are irrelevant.
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        let plan = "\
- [ ] [t-010] Fix the parser crash (no-validate) (coder)
- [ ] [t-011] Investigate the vendored crates (validate) (researcher)
- [ ] [t-012] Read the README and summarize it (researcher)
";
        let generated = PromptBuilder::pregenerate_for_plan_offline(plan, &catalog, &prompts);

        // `(no-validate)` on a coder task → record, no brief.
        assert!(generated.contains(&"t-010-validation-skip.md".to_string()));
        assert!(!prompts.join("t-010-validation.md").exists());
        // `(validate)` on a researcher task → brief, no record.
        assert!(generated.contains(&"t-011-validation.md".to_string()));
        assert!(!prompts.join("t-011-validation-skip.md").exists());
        // `Auto` → brief, no record, whatever the wording of the line is.
        assert!(generated.contains(&"t-012-validation.md".to_string()));
        assert!(!prompts.join("t-012-validation-skip.md").exists());
    }

    /// The directive also survives the plan-line mapping, in every spelling the
    /// grammar accepts, and never leaks into the description body.
    #[test]
    fn test_parse_tasks_from_plan_carries_the_validation_directive() {
        let markdown = "\
# Execution Plan
- [ ] [t-001] Investigate library X (no-validate) (researcher)
- [ ] [t-002] Ship the parser (validate) (coder)
- [ ] [t-003] Migrate the index (coder)
- [ ] [t-004] Sketch the outline (maybe-validate)
- [ ] [t-005] Write the report ( )
";
        let items = PromptBuilder::parse_tasks_from_plan(markdown);
        assert_eq!(items.len(), 5);
        assert_eq!(
            items[0].validation_directive,
            ValidationDirective::NoValidate,
            "the reserved token must survive the mapping"
        );
        assert_eq!(items[0].description, "Investigate library X");
        assert_eq!(items[0].role_hint.as_deref(), Some("researcher"));
        assert_eq!(
            items[1].validation_directive,
            ValidationDirective::Validate,
            "the reserved token must survive the mapping"
        );
        assert_eq!(items[1].description, "Ship the parser");
        // No directive, an unparseable paren group and a blank paren group all stay
        // `Auto` — and none of them turns into a role hint either.
        assert_eq!(items[2].validation_directive, ValidationDirective::Auto);
        assert_eq!(items[3].validation_directive, ValidationDirective::Auto);
        assert_eq!(items[4].validation_directive, ValidationDirective::Auto);
    }

    /// `parse_tasks_from_plan` must see every accepted task-line spelling —
    /// bracketed, parenthesised, starred, numbered and bold-decorated task ids —
    /// and must keep the role-hint split and the "unchecked only"
    /// semantics of that grammar.
    #[test]
    fn test_parse_tasks_from_plan_covers_all_accepted_forms() {
        let markdown = "\
# Execution Plan
- [ ] [t-001] Investigate API contracts (researcher)
- [ ] (t-002) Migrate schema
* [ ] t-003 Add regression tests
- [X] **[t-004]** Docs refreshed
1. [ ] [t-005] Numbered step
- [x] [t-006] Already completed, must be skipped
- plain bullet without a checkbox
";
        let items = PromptBuilder::parse_tasks_from_plan(markdown);
        let ids: Vec<&str> = items.iter().map(|i| i.task_id.as_str()).collect();
        assert_eq!(
            ids,
            vec!["t-001", "t-002", "t-003", "t-005"],
            "unchecked task lines of every accepted spelling must be parsed"
        );
        assert_eq!(items[0].description, "Investigate API contracts");
        assert_eq!(items[0].role_hint.as_deref(), Some("researcher"));
        assert_eq!(items[1].description, "Migrate schema");
        assert_eq!(items[1].role_hint, None);

        // Agreement guard: exactly the ids `Plan::pending_tasks` reports.
        assert_eq!(
            ids,
            crate::plan_parse::unchecked_task_ids(markdown),
            "prompt pre-generation diverges from the on-disk pending set"
        );
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
        assert!(
            blueprint
                .allowed_tools
                .contains(&TOOL_CREATE_PLAN.to_string())
        );
        assert!(
            blueprint
                .allowed_tools
                .contains(&TOOL_READ_FILE.to_string())
        );
        assert!(
            !blueprint
                .allowed_tools
                .contains(&TOOL_WRITE_FILE.to_string())
        );
        assert!(
            blueprint
                .system_prompt
                .contains("Available Agent Archetypes")
        );
        assert!(!blueprint.system_prompt.contains("Active Domain Skills"));
    }

    // The two prompt WRITE sites.
    //
    // `AgentBlueprint::prompt_file_stem` (owns `<task_id>.md`) and
    // `PromptBuilder::validation_prompt_file_name` (owns
    // `<task_id>-validation.md`) both route the task id through
    // `crate::task_id::validate_task_id`, the single grammar authority. A
    // rejected id yields a typed refusal, a WARN and **no file at all**: no
    // substitute name is derived, so downstream prompt/verdict lookups degrade
    // exactly like a missing file.

    /// Task-id spellings that must never become a file name, each with the
    /// variant the grammar authority is expected to report.
    fn hostile_prompt_task_ids() -> Vec<(String, crate::task_id::TaskIdError)> {
        let long = format!("t-{}", "z".repeat(70));
        vec![
            (
                "../escape".to_string(),
                crate::task_id::TaskIdError::DotDotSegment,
            ),
            (
                "a/b".to_string(),
                crate::task_id::TaskIdError::PathSeparator { ch: '/' },
            ),
            ("..".to_string(), crate::task_id::TaskIdError::DotDotSegment),
            (String::new(), crate::task_id::TaskIdError::Empty),
            (
                long,
                crate::task_id::TaskIdError::TooLong {
                    len: 72,
                    max: crate::task_id::MAX_TASK_ID_LEN,
                },
            ),
        ]
    }

    /// A minimal blueprint carrying `raw_id` as its task id.
    fn blueprint_for(raw_id: &str) -> AgentBlueprint {
        AgentBlueprint {
            role_name: "coder_specialist".to_string(),
            reasoning: "gate t-055 probe".to_string(),
            selected_skills: Vec::new(),
            allowed_tools: vec![TOOL_READ_FILE.to_string()],
            system_prompt: "Write a prompt file for this task.".to_string(),
            task_id: Some(raw_id.to_string()),
        }
    }

    /// Every regular file under `root` that does not live under `inside`.
    fn files_outside(root: &Path, inside: &Path) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let mut stack = vec![root.to_path_buf()];
        while let Some(dir) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    stack.push(path);
                } else if !path.starts_with(inside) {
                    found.push(path);
                }
            }
        }
        found
    }

    /// WARN capture counting only the prompt-write events whose message contains
    /// `needle` — the grammar rejections in one test, the failed
    /// validation-brief write in another.
    #[derive(Clone)]
    struct PromptWriteWarns(std::sync::Arc<std::sync::atomic::AtomicUsize>, &'static str);

    struct WarnMessageVisitor<'a> {
        found: &'a mut Option<String>,
    }

    impl tracing::field::Visit for WarnMessageVisitor<'_> {
        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                *self.found = Some(value.to_string());
            }
        }

        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                *self.found = Some(format!("{value:?}"));
            }
        }
    }

    impl tracing::Subscriber for PromptWriteWarns {
        fn enabled(&self, metadata: &tracing::Metadata<'_>) -> bool {
            metadata.level() <= &tracing::Level::WARN
        }

        fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
            Some(tracing::level_filters::LevelFilter::WARN)
        }

        fn register_callsite(
            &self,
            _metadata: &'static tracing::Metadata<'static>,
        ) -> tracing::subscriber::Interest {
            tracing::subscriber::Interest::always()
        }

        fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
            tracing::span::Id::from_u64(1)
        }

        fn record(&self, _span: &tracing::span::Id, _record: &tracing::span::Record<'_>) {}

        fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {}

        fn event(&self, event: &tracing::Event<'_>) {
            if event.metadata().level() != &tracing::Level::WARN {
                return;
            }
            let mut found = None;
            event.record(&mut WarnMessageVisitor { found: &mut found });
            if let Some(message) = found
                && message.contains(self.1)
            {
                self.0.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            }
        }

        fn enter(&self, _span: &tracing::span::Id) {}

        fn exit(&self, _span: &tracing::span::Id) {}
    }

    /// `<task_id>.md`: every hostile id is refused by the grammar authority with
    /// its own typed variant, the save reports `InvalidInput`, warns, and nothing
    /// whatsoever is created — neither outside the prompts dir nor inside it
    /// under a derived name.
    #[test]
    fn test_save_to_disk_refuses_hostile_task_ids_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        let _tracing_lock = crate::orchestrator::workers::TEST_TRACING_MUTEX
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for (raw, expected) in hostile_prompt_task_ids() {
            let bp = blueprint_for(&raw);
            assert_eq!(
                bp.prompt_file_stem().err(),
                Some(expected),
                "{raw:?} must be refused as a prompt file stem"
            );

            let counter = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let result = tracing::subscriber::with_default(
                PromptWriteWarns(counter.clone(), "Rejected task id"),
                || {
                    tracing::callsite::rebuild_interest_cache();
                    bp.save_to_disk(&prompts)
                },
            );
            let err = result.expect_err("a hostile task id must never be persisted");
            assert_eq!(
                err.kind(),
                std::io::ErrorKind::InvalidInput,
                "a rejected task id is reported as InvalidInput, not as a write of some other name"
            );
            assert!(
                counter.load(std::sync::atomic::Ordering::Relaxed) >= 1,
                "the rejection of {raw:?} must be reported with a tracing::warn!"
            );
        }
        drop(_tracing_lock);

        let outside = files_outside(tmp.path(), &prompts);
        assert!(
            outside.is_empty(),
            "no rejected id may write outside the prompts dir: {outside:?}"
        );
        if prompts.is_dir() {
            let stray: Vec<_> = std::fs::read_dir(&prompts)
                .unwrap()
                .flatten()
                .map(|e| e.path())
                .collect();
            assert!(
                stray.is_empty(),
                "a rejected id must not create a substitute file inside the prompts dir: {stray:?}"
            );
        }
    }

    /// `<task_id>.md` happy path: a normal task id (here in its decorated
    /// spelling) normalizes, validates, lands inside the prompts dir and reads
    /// back. A blueprint with **no** task id keeps the documented role-name stem.
    #[test]
    fn test_save_to_disk_round_trips_a_normal_task_id() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        let bp = blueprint_for("[t-055]");
        let path = bp
            .save_to_disk(&prompts)
            .expect("a normal task id must persist its prompt");
        assert_eq!(path, prompts.join("t-055.md"));
        assert_eq!(path.parent(), Some(prompts.as_path()));

        let loaded = AgentBlueprint::load_from_disk(&path).expect("the prompt reads back");
        assert_eq!(loaded.role_name, "coder_specialist");
        assert_eq!(loaded.allowed_tools, vec![TOOL_READ_FILE]);
        assert_eq!(loaded.task_id.as_deref(), Some("[t-055]"));

        let id_less = AgentBlueprint {
            task_id: None,
            ..blueprint_for("unused")
        };
        let adhoc = id_less
            .save_to_disk(&prompts)
            .expect("an ad-hoc JIT blueprint without a task id still persists");
        assert_eq!(adhoc, prompts.join("coder_specialist.md"));
    }

    /// `<task_id>-validation.md`: the name is only ever built from an accepted
    /// id — the rejected spellings are typed refusals, never a mended name.
    #[test]
    fn test_validation_prompt_file_name_refuses_hostile_ids() {
        for (raw, expected) in hostile_prompt_task_ids() {
            assert_eq!(
                PromptBuilder::validation_prompt_file_name(&raw).err(),
                Some(expected),
                "{raw:?} must be refused as a validation prompt file name"
            );
        }
        assert_eq!(
            PromptBuilder::validation_prompt_file_name("t-055"),
            Ok("t-055-validation.md".to_string())
        );
    }

    /// Plan prompt generation: a normal id round-trips through both prompt files
    /// inside the prompts dir, while an over-long plan id contributes neither a
    /// name to the returned list nor a file on disk — and nothing lands outside
    /// the prompts directory.
    #[test]
    fn test_pregenerate_writes_normal_ids_and_skips_rejected_plan_ids() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        let long = format!("t-{}", "z".repeat(70));
        let plan = format!(
            "### Phase 1\n- [ ] [t-055] Implement parser logic (coder)\n- [ ] [{long}] Refactor the widget factory (coder)\n"
        );

        let generated = PromptBuilder::pregenerate_for_plan_offline(&plan, &catalog, &prompts);

        assert!(generated.contains(&"t-055.md".to_string()));
        assert!(generated.contains(&"t-055-validation.md".to_string()));
        assert!(prompts.join("t-055.md").exists());
        assert!(prompts.join("t-055-validation.md").exists());

        assert!(
            !generated.iter().any(|name| name.contains(&long)),
            "a rejected plan id must contribute no file name: {generated:?}"
        );
        assert!(!prompts.join(format!("{long}.md")).exists());
        assert!(!prompts.join(format!("{long}-validation.md")).exists());

        let outside = files_outside(tmp.path(), &prompts);
        assert!(
            outside.is_empty(),
            "no plan id may write outside the prompts dir: {outside:?}"
        );
    }

    // ---- ad-hoc (Steer Arbitrator) no-validation default ---------------------
    //
    // A task delegated by steering has **no plan line**, so the plan-side writer
    // never took a validation decision for it. `record_adhoc_no_validation` is the
    // only code that may record the ad-hoc default, and it must never leak into
    // planned-task semantics.

    /// The ad-hoc record names its **own** provenance and never borrows the plan
    /// wording: one line, exactly `VALIDATION_SKIP_REASON_ADHOC`, while
    /// `VALIDATION_SKIP_REASON` keeps asserting plan provenance untouched.
    #[test]
    fn test_adhoc_record_carries_adhoc_provenance_not_plan_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        let name = PromptBuilder::record_adhoc_no_validation(
            &prompts,
            "steer-task-9",
            Agent::Coder,
            None,
        )
        .expect("a valid ad-hoc id on a validation-eligible role records the ad-hoc default");
        assert_eq!(name, "steer-task-9-validation-skip.md");
        assert_eq!(
            std::fs::read_to_string(prompts.join(&name))
                .expect("ad-hoc skip record")
                .as_str(),
            format!("{VALIDATION_SKIP_REASON_ADHOC}\n"),
            "the record is exactly one line carrying the ad-hoc reason"
        );
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "steer-task-9").as_deref(),
            Some(VALIDATION_SKIP_REASON_ADHOC)
        );
        // Provenance honesty, both directions.
        assert!(
            !VALIDATION_SKIP_REASON_ADHOC.contains("Plan directive"),
            "an ad-hoc task has no plan line, so its record must not claim a plan directive"
        );
        assert!(
            VALIDATION_SKIP_REASON.contains("Plan directive"),
            "the plan-directive reason stays pinned for planned tasks"
        );
        // A decorated steer id normalizes to the same record name (grammar parity
        // with the blueprint writer), never to a mended name.
        assert_eq!(
            PromptBuilder::record_adhoc_no_validation(
                &prompts,
                "[steer-task-10]",
                Agent::Researcher,
                None,
            )
            .as_deref(),
            Some("steer-task-10-validation-skip.md")
        );
        // The brief really is absent afterwards — the only state a record explains.
        assert!(crate::agents::validation::probe_validation_brief(&prompts, "steer-task-9").is_absent());
    }

    /// `planner` / `validator` are role-level opt-outs: the read side never probes
    /// for them, so an ad-hoc record would be inert and would contradict the
    /// documented "role-level opt-outs are never briefed" rule. Nothing is written.
    #[test]
    fn test_adhoc_record_is_not_authored_for_role_exempt_roles() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        for agent in [Agent::Planner, Agent::Validator] {
            assert_eq!(
                PromptBuilder::record_adhoc_no_validation(
                    &prompts,
                    &format!("steer-task-{}", agent.as_str()),
                    agent,
                    None,
                ),
                None,
                "{agent} is a role-level opt-out and must get no ad-hoc record"
            );
            assert_eq!(
                PromptBuilder::validation_skip_record(&prompts, &format!("steer-task-{}", agent.as_str())),
                None,
                "{agent} must have no record on disk"
            );
        }
        let files: Vec<String> = std::fs::read_dir(&prompts)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            files.iter().all(|name| !name.ends_with("-validation-skip.md")),
            "role-exempt ad-hoc tasks must create no skip record at all: {files:?}"
        );
        // A validation-eligible role in the same directory does get one, so the
        // absence above is the role rule, not a broken fixture.
        assert!(
            PromptBuilder::record_adhoc_no_validation(
                &prompts,
                "steer-task-coder",
                Agent::Coder,
                None,
            )
            .is_some()
        );
    }

    /// **The anti-typo / anti-collision guard.** An id that is a line of the active
    /// plan — pending *or* already checked, in either spelling — is a **planned**
    /// task: the ad-hoc default must not exempt it, and the plan's own records must
    /// survive byte-for-byte. An id absent from the plan still gets the record, so
    /// the guard is keyed on plan membership, not on the plan existing at all.
    #[test]
    fn test_adhoc_record_is_not_authored_for_an_id_that_is_a_plan_line() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        std::fs::create_dir_all(&prompts).expect("prompts dir");

        let plan = "# Execution Plan\n\n### Phase 1\n\
                    - [ ] [t-001] Fix the parser (coder)\n\
                    - [x] [t-002] Audit the code structure (researcher)\n";
        // A plan-authored brief for the pending line, which must not be destroyed.
        let brief = prompts.join("t-001-validation.md");
        std::fs::write(&brief, "audit this deliverable\n").expect("plan brief");

        for id in ["t-001", "t-002"] {
            assert_eq!(
                PromptBuilder::record_adhoc_no_validation(&prompts, id, Agent::Coder, Some(plan)),
                None,
                "{id} is a plan line, so no ad-hoc exemption may be recorded for it"
            );
            assert_eq!(
                PromptBuilder::validation_skip_record(&prompts, id),
                None,
                "{id} must keep plan semantics, not an ad-hoc record"
            );
        }
        assert_eq!(
            std::fs::read_to_string(&brief).expect("plan brief survives"),
            "audit this deliverable\n",
            "the collision guard must neither exempt a planned task nor destroy its brief"
        );

        // The very same id DOES get the record when no plan mentions it, proving the
        // guard is plan membership rather than the plan's mere existence.
        assert_eq!(
            PromptBuilder::record_adhoc_no_validation(
                &prompts,
                "t-001",
                Agent::Coder,
                Some("# Execution Plan\n\n- [ ] [t-999] Something else (coder)\n"),
            )
            .as_deref(),
            Some("t-001-validation-skip.md")
        );
    }

    /// Mutual exclusivity: the ad-hoc record replaces a stale brief from an earlier
    /// plan, and when that brief cannot be deleted **no record is authored** — the
    /// read side ranks a present brief above a record, so recording a skip on top of
    /// one would misreport what the read side does (fail closed, never fail open).
    #[test]
    fn test_adhoc_record_replaces_a_stale_brief_and_never_coexists() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        std::fs::create_dir_all(&prompts).expect("prompts dir");

        // Deletable stale brief → gone, record written, brief really absent.
        std::fs::write(prompts.join("steer-task-11-validation.md"), "stale\n").expect("stale brief");
        assert_eq!(
            PromptBuilder::record_adhoc_no_validation(&prompts, "steer-task-11", Agent::Coder, None)
                .as_deref(),
            Some("steer-task-11-validation-skip.md")
        );
        assert!(
            !prompts.join("steer-task-11-validation.md").exists(),
            "a stale brief must not coexist with the ad-hoc record"
        );
        assert!(
            crate::agents::validation::probe_validation_brief(&prompts, "steer-task-11").is_absent()
        );

        // Undeletable brief (the destination is a directory) → no record at all, and
        // the contradicting pair is never left on disk.
        std::fs::create_dir(prompts.join("steer-task-12-validation.md")).expect("blocking dir");
        assert_eq!(
            PromptBuilder::record_adhoc_no_validation(&prompts, "steer-task-12", Agent::Coder, None),
            None,
            "a brief that cannot be deleted must not be papered over with a record"
        );
        assert!(
            prompts.join("steer-task-12-validation.md").exists(),
            "the undeletable brief stays where the read side can still see it"
        );
        assert!(
            !prompts.join("steer-task-12-validation-skip.md").exists(),
            "no ad-hoc record may be authored while a brief survives"
        );
        assert_eq!(
            PromptBuilder::validation_skip_record(&prompts, "steer-task-12"),
            None,
            "the task stays fail closed"
        );
    }

    /// Self-healing in the other direction (E5): when an id that earlier got an
    /// ad-hoc record later **is** a plan line, `create_plan`'s writer restores plan
    /// semantics — the ad-hoc record is dropped and the brief authored.
    #[test]
    fn test_adhoc_record_is_dropped_when_the_id_later_becomes_a_plan_line() {
        let catalog = Catalog::new();
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");

        assert!(
            PromptBuilder::record_adhoc_no_validation(&prompts, "t-077", Agent::Coder, None)
                .is_some()
        );
        assert!(prompts.join("t-077-validation-skip.md").exists());

        let generated = PromptBuilder::pregenerate_for_plan_offline(
            "- [ ] [t-077] Implement the widget factory (coder)\n",
            &catalog,
            &prompts,
        );
        assert!(generated.contains(&"t-077-validation.md".to_string()));
        assert!(prompts.join("t-077-validation.md").exists());
        assert!(
            !prompts.join("t-077-validation-skip.md").exists(),
            "a plan-authored task must not inherit the earlier ad-hoc exemption"
        );
        assert_eq!(PromptBuilder::validation_skip_record(&prompts, "t-077"), None);
    }

    /// An id the grammar authority refuses can never be given an exemption: no
    /// record name is returned, no file is created, and nothing lands outside the
    /// prompts directory. Same hostile table the blueprint and plan writers use,
    /// plus the shapes specific to a steer-authored id.
    #[test]
    fn test_adhoc_record_refuses_grammar_rejected_ids_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let prompts = tmp.path().join("prompts");
        let long = "t-".to_string() + &"z".repeat(63); // 65 chars > MAX_TASK_ID_LEN
        assert_eq!(long.chars().count(), crate::task_id::MAX_TASK_ID_LEN + 1);

        for raw in ["../x", "a/b", "..", ".hidden", "", long.as_str()] {
            assert_eq!(
                PromptBuilder::record_adhoc_no_validation(&prompts, raw, Agent::Coder, None),
                None,
                "{raw:?} must be refused and never gain an ad-hoc exemption"
            );
        }
        let strays: Vec<String> = std::fs::read_dir(&prompts)
            .map(|entries| {
                entries
                    .flatten()
                    .map(|e| e.file_name().to_string_lossy().into_owned())
                    .collect()
            })
            .unwrap_or_default();
        assert!(
            strays.is_empty(),
            "a refused ad-hoc id must create no file of any kind: {strays:?}"
        );
        assert!(
            files_outside(tmp.path(), &prompts).is_empty(),
            "a refused ad-hoc id must never write outside the prompts dir"
        );
    }
}
