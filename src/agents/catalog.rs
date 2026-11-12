//! Skill & Agent Catalog — manages discovery and resolution of skills and archetypes.
//!
//! Precedence order:
//! 1. Project-level: `<workspace>/skills/*.md`, `<workspace>/skills/*/SKILL.md`, `<workspace>/ARCHETYPES.md`
//! 2. User-level: `~/.marmel/skills/*.md`, `~/.marmel/skills/*/SKILL.md`, `~/.marmel/ARCHETYPES.md`
//! 3. Built-in: embedded base skills compiled in at build time.
//!
//! `AGENTS.md` is *not* part of the catalog: it holds free-form project instructions
//! (git workflow, required checks, conventions) that are injected verbatim into the
//! agents' prompts. See [`load_project_instructions`].

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::tool_names::{
    TOOL_CREATE_PLAN, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT, TOOL_PTY_CLOSE,
    TOOL_PTY_LIST, TOOL_PTY_READ, TOOL_PTY_SPAWN, TOOL_PTY_WRITE, TOOL_READ_FILE, TOOL_REBIRTH,
    TOOL_REPLACE, TOOL_REPLY_TO_ARBITRATOR, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
};

/// File holding custom archetype definitions (`## <Role>` sections).
pub const ARCHETYPES_FILE: &str = "ARCHETYPES.md";
/// File holding free-form project instructions for agents.
pub const AGENTS_FILE: &str = "AGENTS.md";
/// Instruction file names, in priority order. The first one that exists in a
/// directory is used, so a repository that only ships `CLAUDE.md` or `GEMINI.md`
/// still gets its rules applied while `AGENTS.md` always wins.
pub const INSTRUCTION_FILES: [&str; 3] = [AGENTS_FILE, "CLAUDE.md", "GEMINI.md"];
/// Upper bound for the nested instruction text appended to a `read_file` result.
const MAX_NESTED_INSTRUCTIONS_BYTES: usize = 8 * 1024;
/// Upper bound for the injected instruction text, to protect the context window.
const MAX_INSTRUCTIONS_BYTES: usize = 32 * 1024;

/// First existing instruction file in `dir` (see [`INSTRUCTION_FILES`]).
fn instruction_file_in(dir: &Path) -> Option<PathBuf> {
    INSTRUCTION_FILES
        .iter()
        .map(|name| dir.join(name))
        .find(|p| p.is_file())
}

/// Nested instructions that apply to `file`: every instruction file found in the
/// directories between `workspace_root` (exclusive, it is already in the prompt)
/// and the file's own directory, outermost first so the closest one reads last.
///
/// Returns `None` when `file` is outside the workspace or no nested file exists.
pub fn nested_instructions_for(workspace_root: &Path, file: &Path) -> Option<String> {
    let rel_dir = file.strip_prefix(workspace_root).ok()?.parent()?;
    let mut dir = workspace_root.to_path_buf();
    let mut parts: Vec<String> = Vec::new();
    for comp in rel_dir.components() {
        dir.push(comp);
        let Some(path) = instruction_file_in(&dir) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        let shown = path.strip_prefix(workspace_root).unwrap_or(&path);
        parts.push(format!("## {}\n{text}", shown.display()));
    }
    if parts.is_empty() {
        return None;
    }
    let mut body = parts.join("\n\n");
    if body.len() > MAX_NESTED_INSTRUCTIONS_BYTES {
        let mut end = MAX_NESTED_INSTRUCTIONS_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n[... nested instructions truncated ...]");
    }
    Some(format!(
        "\n\n[Directory-specific instructions that apply to this file; they take precedence over the project-level rules]\n{body}"
    ))
}

/// Load project instructions from `~/.marmel/` (first) and `<workspace_root>`
/// (second) and render them as a prompt section. In each directory the first
/// existing file of [`INSTRUCTION_FILES`] is used.
///
/// Returns `None` when neither file exists or both are blank. The text is included
/// verbatim (it is not parsed for archetypes) and truncated at a char boundary if it
/// exceeds [`MAX_INSTRUCTIONS_BYTES`].
pub fn load_project_instructions(workspace_root: &Path) -> Option<String> {
    let mut sources: Vec<PathBuf> = Vec::new();
    if let Some(home) = crate::config::home_dir()
        && let Some(user) = instruction_file_in(&home.join(".marmel"))
    {
        sources.push(user);
    }
    if let Some(project) = instruction_file_in(workspace_root)
        && !sources.contains(&project)
    {
        sources.push(project);
    }

    let mut body = String::new();
    for path in sources {
        let Ok(raw_text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let normalized = raw_text.replace("\r\n", "\n");
        let text = normalized.trim();
        if text.is_empty() {
            continue;
        }
        if !body.is_empty() {
            body.push_str("\n\n");
        }
        body.push_str(text);
    }
    if body.is_empty() {
        return None;
    }
    if body.len() > MAX_INSTRUCTIONS_BYTES {
        let mut end = MAX_INSTRUCTIONS_BYTES;
        while !body.is_char_boundary(end) {
            end -= 1;
        }
        body.truncate(end);
        body.push_str("\n[... AGENTS.md truncated ...]");
    }
    Some(format!(
        "\n\n# Project Instructions (AGENTS.md)\nThese rules are mandatory for all work in this repository.\n\n{body}\n"
    ))
}

/// Origin of a loaded skill or archetype.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum SkillSource {
    Builtin,
    User(PathBuf),
    Project(PathBuf),
}

/// A discrete unit of capability, domain knowledge, or operational guidelines.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Skill {
    pub id: String,
    pub name: String,
    pub description: String,
    pub content: String,
    pub suggested_tools: Vec<String>,
    pub source: SkillSource,
}

/// A configured agent archetype template.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentArchetype {
    pub id: String,
    pub name: String,
    pub description: String,
    pub default_skills: Vec<String>,
    pub default_tools: Vec<String>,
    pub source: SkillSource,
}

/// Central catalog containing all discovered skills and agent archetypes.
#[derive(Debug, Clone, Default)]
pub struct Catalog {
    skills: HashMap<String, Skill>,
    agents: HashMap<String, AgentArchetype>,
}

impl Catalog {
    /// Create a catalog with only the built-in base skills and archetypes.
    pub fn new() -> Self {
        let mut catalog = Self::default();
        catalog.register_builtins();
        catalog
    }

    /// Discover skills and archetypes from built-ins, user home, and project workspace.
    pub fn discover(workspace_root: &Path) -> Self {
        let mut catalog = Self::new();

        // 1. User level (~/.marmel/skills/ and ~/.marmel/ARCHETYPES.md)
        if let Some(home) = crate::config::home_dir() {
            let user_marmel = home.join(".marmel");
            let user_skills_dir = user_marmel.join("skills");
            if user_skills_dir.is_dir() {
                catalog.load_skills_from_dir(&user_skills_dir, SkillSource::User);
            }
            let user_agents_file = user_marmel.join(ARCHETYPES_FILE);
            if user_agents_file.is_file() {
                catalog.load_agents_from_file(
                    &user_agents_file,
                    SkillSource::User(user_agents_file.clone()),
                );
            }
        }

        // 2. Project level (<workspace>/skills/ and <workspace>/ARCHETYPES.md)
        let project_skills_dir = workspace_root.join("skills");
        if project_skills_dir.is_dir() {
            catalog.load_skills_from_dir(&project_skills_dir, SkillSource::Project);
        }
        let project_agents_file = workspace_root.join(ARCHETYPES_FILE);
        if project_agents_file.is_file() {
            catalog.load_agents_from_file(
                &project_agents_file,
                SkillSource::Project(project_agents_file.clone()),
            );
        }

        catalog
    }

    fn register_builtins(&mut self) {
        self.register_skill(Self::parse_skill_markdown(
            "clean_code",
            crate::prompts::SKILL_CLEAN_CODE,
            SkillSource::Builtin,
        ));
        self.register_skill(Self::parse_skill_markdown(
            "debugging",
            crate::prompts::SKILL_DEBUGGING,
            SkillSource::Builtin,
        ));
        self.register_skill(Self::parse_skill_markdown(
            "research",
            crate::prompts::SKILL_RESEARCH,
            SkillSource::Builtin,
        ));
        self.register_skill(Self::parse_skill_markdown(
            "verification",
            crate::prompts::SKILL_VERIFICATION,
            SkillSource::Builtin,
        ));
        self.register_skill(Self::parse_skill_markdown(
            "testing",
            crate::prompts::SKILL_TESTING,
            SkillSource::Builtin,
        ));
        self.register_skill(Self::parse_skill_markdown(
            "git_workflow",
            crate::prompts::SKILL_GIT_WORKFLOW,
            SkillSource::Builtin,
        ));

        self.register_agent(AgentArchetype {
            id: "coder".to_string(),
            name: "Software Engineer".to_string(),
            description: "System architecture, modular design, clean coding, and test suites."
                .to_string(),
            default_skills: vec![
                "clean_code".to_string(),
                "testing".to_string(),
                "git_workflow".to_string(),
            ],
            default_tools: vec![
                TOOL_READ_FILE.to_string(),
                TOOL_WRITE_FILE.to_string(),
                TOOL_REPLACE.to_string(),
                TOOL_RUN_COMMAND.to_string(),
                TOOL_GREP_SEARCH.to_string(),
                TOOL_GLOB.to_string(),
                TOOL_REBIRTH.to_string(),
                TOOL_REPLY_TO_ARBITRATOR.to_string(),
            ],
            source: SkillSource::Builtin,
        });

        self.register_agent(AgentArchetype {
            id: "debugger".to_string(),
            name: "Systems Debugger".to_string(),
            description:
                "Crash forensics, root-cause isolation, interactive stepping, and surgical fixes."
                    .to_string(),
            default_skills: vec![
                "debugging".to_string(),
                "testing".to_string(),
                "git_workflow".to_string(),
            ],
            default_tools: vec![
                TOOL_READ_FILE.to_string(),
                TOOL_REPLACE.to_string(),
                TOOL_RUN_COMMAND.to_string(),
                TOOL_PTY_SPAWN.to_string(),
                TOOL_PTY_WRITE.to_string(),
                TOOL_PTY_READ.to_string(),
                TOOL_PTY_CLOSE.to_string(),
                TOOL_PTY_LIST.to_string(),
                TOOL_GREP_SEARCH.to_string(),
                TOOL_GLOB.to_string(),
                TOOL_REBIRTH.to_string(),
                TOOL_REPLY_TO_ARBITRATOR.to_string(),
            ],
            source: SkillSource::Builtin,
        });

        self.register_agent(AgentArchetype {
            id: "researcher".to_string(),
            name: "Information Researcher".to_string(),
            description: "Documentation lookup, codebase reconnaissance, and factual synthesis."
                .to_string(),
            default_skills: vec!["research".to_string()],
            default_tools: vec![
                TOOL_READ_FILE.to_string(),
                TOOL_GREP_SEARCH.to_string(),
                TOOL_GLOB.to_string(),
                TOOL_RUN_COMMAND.to_string(),
                TOOL_WRITE_FILE.to_string(),
                TOOL_REBIRTH.to_string(),
                TOOL_REPLY_TO_ARBITRATOR.to_string(),
            ],
            source: SkillSource::Builtin,
        });

        self.register_agent(AgentArchetype {
            id: "validator".to_string(),
            name: "Quality Auditor".to_string(),
            description:
                "Independent quality verification, code inspection, and formal verdict submission."
                    .to_string(),
            default_skills: vec!["verification".to_string(), "git_workflow".to_string()],
            default_tools: vec![
                TOOL_READ_FILE.to_string(),
                TOOL_GREP_SEARCH.to_string(),
                TOOL_GLOB.to_string(),
                TOOL_LEAVE_VERDICT.to_string(),
                TOOL_REPLY_TO_ARBITRATOR.to_string(),
            ],
            source: SkillSource::Builtin,
        });

        self.register_agent(AgentArchetype {
            id: "generalist".to_string(),
            name: "Supreme Polymath".to_string(),
            description: "Dense reasoning, cross-domain logic, and holistic problem solving."
                .to_string(),
            default_skills: vec![
                "clean_code".to_string(),
                "research".to_string(),
                "testing".to_string(),
                "git_workflow".to_string(),
            ],
            default_tools: vec![
                TOOL_READ_FILE.to_string(),
                TOOL_WRITE_FILE.to_string(),
                TOOL_REPLACE.to_string(),
                TOOL_RUN_COMMAND.to_string(),
                TOOL_GREP_SEARCH.to_string(),
                TOOL_GLOB.to_string(),
                TOOL_REBIRTH.to_string(),
                TOOL_REPLY_TO_ARBITRATOR.to_string(),
            ],
            source: SkillSource::Builtin,
        });

        self.register_agent(AgentArchetype {
            id: "planner".to_string(),
            name: "Strategic Planner".to_string(),
            description:
                "Mission architecture, dependency-ordered phased planning, and task decomposition."
                    .to_string(),
            default_skills: Vec::new(),
            default_tools: vec![
                TOOL_READ_FILE.to_string(),
                TOOL_GREP_SEARCH.to_string(),
                TOOL_GLOB.to_string(),
                TOOL_CREATE_PLAN.to_string(),
                TOOL_REBIRTH.to_string(),
                TOOL_REPLY_TO_ARBITRATOR.to_string(),
            ],
            source: SkillSource::Builtin,
        });
    }

    pub fn register_skill(&mut self, skill: Skill) {
        self.skills.insert(skill.id.clone(), skill);
    }

    pub fn register_agent(&mut self, agent: AgentArchetype) {
        self.agents.insert(agent.id.clone(), agent);
    }

    pub fn get_skill(&self, id: &str) -> Option<&Skill> {
        self.skills.get(id)
    }

    pub fn get_agent(&self, id: &str) -> Option<&AgentArchetype> {
        self.agents.get(id)
    }

    pub fn skills(&self) -> &HashMap<String, Skill> {
        &self.skills
    }

    pub fn agents(&self) -> &HashMap<String, AgentArchetype> {
        &self.agents
    }

    /// Recursively load skills from a directory. Looks for `*.md` files and `*/SKILL.md`.
    pub fn load_skills_from_dir<F>(&mut self, dir: &Path, source_ctor: F)
    where
        F: Fn(PathBuf) -> SkillSource,
    {
        let entries = match std::fs::read_dir(dir) {
            Ok(e) => e,
            Err(_) => return,
        };

        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                let sub_skill = path.join("SKILL.md");
                if sub_skill.is_file()
                    && let Ok(content) = std::fs::read_to_string(&sub_skill)
                {
                    let id = path
                        .file_name()
                        .and_then(|s| s.to_str())
                        .unwrap_or("unknown");
                    let skill = Self::parse_skill_markdown(id, &content, source_ctor(sub_skill));
                    self.register_skill(skill);
                }
            } else if path.is_file() && path.extension().and_then(|s| s.to_str()) == Some("md") {
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("unknown")
                    .to_string();
                if stem.eq_ignore_ascii_case("README") {
                    continue;
                }
                if let Ok(content) = std::fs::read_to_string(&path) {
                    let skill = Self::parse_skill_markdown(&stem, &content, source_ctor(path));
                    self.register_skill(skill);
                }
            }
        }
    }

    /// Load agent definitions from an `AGENTS.md` file.
    pub fn load_agents_from_file(&mut self, file: &Path, source: SkillSource) {
        if let Ok(content) = std::fs::read_to_string(file) {
            for agent in Self::parse_agents_markdown(&content, source) {
                self.register_agent(agent);
            }
        }
    }

    /// Parse markdown with optional YAML frontmatter (`--- ... ---`) into a [`Skill`].
    pub fn parse_skill_markdown(default_id: &str, raw: &str, source: SkillSource) -> Skill {
        let trimmed = raw.trim();
        if let Some(stripped) = trimmed.strip_prefix("---")
            && let Some(end_idx) = stripped.find("---")
        {
            let frontmatter = &stripped[..end_idx];
            let body = stripped[end_idx + 3..].trim().to_string();

            let mut id = default_id.to_string();
            let mut name = default_id.to_string();
            let mut description = String::new();
            let mut suggested_tools = Vec::new();

            let mut in_tools = false;
            for line in frontmatter.lines() {
                let l = line.trim();
                if l.is_empty() {
                    continue;
                }
                if in_tools {
                    if l.starts_with('-') || l.starts_with('*') {
                        let tool = l.trim_start_matches(['-', '*', ' ']).trim();
                        if !tool.is_empty() {
                            suggested_tools.push(tool.to_string());
                        }
                        continue;
                    } else if !l.contains(':') {
                        // Continuation of list
                        let tool = l.trim();
                        if !tool.is_empty() {
                            suggested_tools.push(tool.to_string());
                        }
                        continue;
                    } else {
                        in_tools = false;
                    }
                }

                if let Some((k, v)) = l.split_once(':') {
                    let key = k.trim().to_ascii_lowercase();
                    let val = v.trim();
                    match key.as_str() {
                        "id" => {
                            if !val.is_empty() {
                                id = val.to_string();
                            }
                        }
                        "name" => {
                            if !val.is_empty() {
                                name = val.to_string();
                            }
                        }
                        "description" => {
                            description = val.to_string();
                        }
                        "suggested_tools" | "tools" => {
                            if val.is_empty() {
                                in_tools = true;
                            } else {
                                for t in val.split(',') {
                                    let tool = t.trim().trim_matches(|c| c == '[' || c == ']');
                                    if !tool.is_empty() {
                                        suggested_tools.push(tool.to_string());
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }

            return Skill {
                id,
                name,
                description,
                content: body,
                suggested_tools,
                source,
            };
        }

        // Fallback: no frontmatter. Extract first header as name.
        let mut name = default_id.to_string();
        for line in raw.lines() {
            let l = line.trim();
            if l.starts_with('#') {
                name = l.trim_start_matches(['#', ' ']).to_string();
                break;
            }
        }

        Skill {
            id: default_id.to_string(),
            name,
            description: String::new(),
            content: raw.to_string(),
            suggested_tools: Vec::new(),
            source,
        }
    }

    /// Parse markdown containing multiple `## <Agent Name>` or `### <Agent Name>` sections.
    pub fn parse_agents_markdown(raw: &str, source: SkillSource) -> Vec<AgentArchetype> {
        let mut agents = Vec::new();
        let mut current_id = String::new();
        let mut current_name = String::new();
        let mut current_desc = String::new();
        let mut current_skills = Vec::new();
        let mut current_tools = Vec::new();

        let finalize = |id: &mut String,
                        name: &mut String,
                        desc: &mut String,
                        skills: &mut Vec<String>,
                        tools: &mut Vec<String>,
                        out: &mut Vec<AgentArchetype>| {
            if !id.is_empty() {
                out.push(AgentArchetype {
                    id: std::mem::take(id),
                    name: if name.is_empty() {
                        "Custom Agent".to_string()
                    } else {
                        std::mem::take(name)
                    },
                    description: std::mem::take(desc),
                    default_skills: std::mem::take(skills),
                    default_tools: std::mem::take(tools),
                    source: source.clone(),
                });
            }
        };

        for line in raw.lines() {
            let l = line.trim();
            if l.starts_with("## ") || l.starts_with("### ") {
                finalize(
                    &mut current_id,
                    &mut current_name,
                    &mut current_desc,
                    &mut current_skills,
                    &mut current_tools,
                    &mut agents,
                );
                let header = l.trim_start_matches('#').trim();
                let clean_id = header.to_ascii_lowercase().replace([' ', '-'], "_");
                current_id = clean_id;
                current_name = header.to_string();
            } else if let Some((k, v)) = l.split_once(':') {
                let key = k.trim().to_ascii_lowercase();
                let val = v.trim();
                match key.as_str() {
                    "description" => current_desc = val.to_string(),
                    "skills" => {
                        for s in val.split(',') {
                            let item = s.trim();
                            if !item.is_empty() {
                                current_skills.push(item.to_string());
                            }
                        }
                    }
                    "tools" => {
                        for t in val.split(',') {
                            let item = t.trim();
                            if !item.is_empty() {
                                current_tools.push(item.to_string());
                            }
                        }
                    }
                    _ => {}
                }
            }
        }

        finalize(
            &mut current_id,
            &mut current_name,
            &mut current_desc,
            &mut current_skills,
            &mut current_tools,
            &mut agents,
        );

        agents
    }

    /// Formats an inventory of available skills and tools for prompt injection.
    pub fn format_catalog_for_prompt(&self) -> String {
        let mut out = String::from("## Available Skills in Catalog\n");
        let mut sorted_skills: Vec<&Skill> = self.skills.values().collect();
        sorted_skills.sort_by(|a, b| a.id.cmp(&b.id));

        for skill in sorted_skills {
            out.push_str(&format!(
                "- **`{}`** ({}): {}\n  Suggested Tools: {}\n",
                skill.id,
                skill.name,
                if skill.description.is_empty() {
                    "Domain guidance"
                } else {
                    &skill.description
                },
                if skill.suggested_tools.is_empty() {
                    "None".to_string()
                } else {
                    skill.suggested_tools.join(", ")
                }
            ));
        }

        out.push_str("\n## Available Tools in Environment\n");
        for tool in crate::types::ToolDef::default_tools() {
            out.push_str(&format!(
                "- `{}`: {}\n",
                tool.function.name, tool.function.description
            ));
        }

        out
    }

    /// Formats the catalog's agent archetypes and their tool capabilities for the Planner's strategic context.
    /// Excludes internal checkpoint/control tools and micro-skills to keep the planner focused on high-level architecture.
    pub fn format_agents_for_planner(&self, cfg: Option<&crate::config::Config>) -> String {
        let mut out = String::from(
            "## Available Agent Archetypes & Tool Capabilities (from ARCHETYPES.md & defaults)\n",
        );
        let mut sorted_agents: Vec<&AgentArchetype> = self.agents.values().collect();
        sorted_agents.sort_by(|a, b| a.id.cmp(&b.id));

        for arch in sorted_agents {
            out.push_str(&format!(
                "- **`{}`** ({}): {}\n",
                arch.id, arch.name, arch.description
            ));

            let operational_tools: Vec<String> = arch
                .default_tools
                .iter()
                .filter(|t| !matches!(t.as_str(), TOOL_REBIRTH | TOOL_REPLY_TO_ARBITRATOR))
                .map(|t| format!("`{t}`"))
                .collect();
            if !operational_tools.is_empty() {
                out.push_str(&format!(
                    "  - Built-in tools: {}\n",
                    operational_tools.join(", ")
                ));
            }

            if let Some(cfg) = cfg {
                let mcp_servers = cfg.effective_specialist_mcp_servers(&arch.id);
                let allowed_mcp = crate::harness::allowed_mcp_tools(&mcp_servers);
                if !allowed_mcp.is_empty() {
                    let mcp_tool_names: Vec<String> = allowed_mcp
                        .iter()
                        .map(|t| format!("`{}`", t.qualified_name()))
                        .collect();
                    out.push_str(&format!(
                        "  - Allowed MCP tools: {}\n",
                        mcp_tool_names.join(", ")
                    ));
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_catalog_builtins_loaded() {
        let catalog = Catalog::new();
        assert!(catalog.get_skill("clean_code").is_some());
        assert!(catalog.get_skill("debugging").is_some());
        assert!(catalog.get_skill("research").is_some());
        assert!(catalog.get_skill("verification").is_some());
        assert!(catalog.get_skill("testing").is_some());
        assert!(catalog.get_skill("git_workflow").is_some());

        assert!(catalog.get_agent("coder").is_some());
        assert!(catalog.get_agent("debugger").is_some());
    }

    /// The built-in `git_workflow` skill must be registered from its front matter
    /// and wired to exactly the roles that can act on git work.
    #[test]
    fn git_workflow_skill_is_registered_and_wired_to_git_roles() {
        let catalog = Catalog::new();
        let skill = catalog
            .get_skill("git_workflow")
            .expect("git_workflow must be a built-in skill");
        assert!(matches!(skill.source, SkillSource::Builtin));
        assert_eq!(
            skill.name,
            "Git Workflow, Worktree Isolation & Commit Discipline"
        );
        assert!(!skill.description.trim().is_empty());
        assert_eq!(
            skill.suggested_tools,
            [
                TOOL_RUN_COMMAND,
                TOOL_READ_FILE,
                TOOL_WRITE_FILE,
                TOOL_REPLACE,
                TOOL_GREP_SEARCH
            ]
        );
        assert!(skill.content.contains("WORKTREE ISOLATION"));

        for role in ["coder", "debugger", "generalist", "validator"] {
            assert!(
                catalog
                    .get_agent(role)
                    .expect("role must exist")
                    .default_skills
                    .contains(&"git_workflow".to_string()),
                "{role} must carry the git workflow skill"
            );
        }
        for role in ["researcher", "planner"] {
            assert!(
                !catalog
                    .get_agent(role)
                    .expect("role must exist")
                    .default_skills
                    .contains(&"git_workflow".to_string()),
                "{role} must not be given git work by default"
            );
        }
    }

    #[test]
    fn test_parse_skill_markdown_with_frontmatter() {
        let md = r#"---
id: rust_safety
name: Rust Safety Patterns
description: Enforce lifetime and borrow checker discipline.
suggested_tools:
  - read_file
  - replace
---

# Rust Safety
Always avoid unsafe where possible.
"#;
        let skill = Catalog::parse_skill_markdown("fallback", md, SkillSource::Builtin);
        assert_eq!(skill.id, "rust_safety");
        assert_eq!(skill.name, "Rust Safety Patterns");
        assert_eq!(skill.suggested_tools, vec![TOOL_READ_FILE, TOOL_REPLACE]);
        assert!(skill.content.contains("Always avoid unsafe"));
    }

    #[test]
    fn test_parse_agents_markdown() {
        let md = r#"
## Rust Specialist
description: Expert in high-performance Rust
skills: clean_code, testing
tools: read_file, write_file, run_command

## Security Auditor
description: Inspects vulnerabilities
skills: verification
tools: read_file, grep_search
"#;
        let agents = Catalog::parse_agents_markdown(md, SkillSource::Builtin);
        assert_eq!(agents.len(), 2);
        assert_eq!(agents[0].id, "rust_specialist");
        assert_eq!(agents[0].default_skills, vec!["clean_code", "testing"]);
        assert_eq!(agents[1].id, "security_auditor");
        assert_eq!(
            agents[1].default_tools,
            vec![TOOL_READ_FILE, TOOL_GREP_SEARCH]
        );
    }
}

#[cfg(test)]
mod project_instructions_tests {
    use super::*;

    #[test]
    fn agents_md_is_instructions_not_archetypes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join(AGENTS_FILE),
            "# Rules\n\n## Git\ndescription: not a role\nRun clippy.\n",
        )
        .unwrap();
        let catalog = Catalog::discover(tmp.path());
        assert!(catalog.get_agent("git").is_none());
        let section = load_project_instructions(tmp.path()).expect("instructions");
        assert!(section.contains("Run clippy."));
        assert!(section.contains("Project Instructions"));
    }

    #[test]
    fn claude_md_is_used_as_fallback_but_agents_md_wins() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("CLAUDE.md"), "claude rule").unwrap();
        assert_eq!(
            instruction_file_in(tmp.path()),
            Some(tmp.path().join("CLAUDE.md"))
        );
        std::fs::write(tmp.path().join(AGENTS_FILE), "agents rule").unwrap();
        assert_eq!(
            instruction_file_in(tmp.path()),
            Some(tmp.path().join(AGENTS_FILE))
        );
    }

    #[test]
    fn nested_instructions_are_outermost_first_and_skip_the_root() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("a/b")).unwrap();
        std::fs::write(root.join(AGENTS_FILE), "root rule").unwrap();
        std::fs::write(root.join("a").join(AGENTS_FILE), "a rule").unwrap();
        std::fs::write(root.join("a/b").join("GEMINI.md"), "b rule").unwrap();
        let out = nested_instructions_for(root, &root.join("a/b/file.rs")).unwrap();
        assert!(!out.contains("root rule"));
        let a = out.find("a rule").unwrap();
        let b = out.find("b rule").unwrap();
        assert!(a < b);
        assert!(nested_instructions_for(root, &root.join("file.rs")).is_none());
        assert!(nested_instructions_for(root, Path::new("/elsewhere/x.rs")).is_none());
    }

    #[test]
    fn missing_or_blank_agents_md_yields_none_for_project() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join(AGENTS_FILE), "  \n").unwrap();
        let out = load_project_instructions(tmp.path());
        if let Some(text) = out {
            // Only a user-level ~/.marmel/AGENTS.md may contribute here.
            assert!(!text.contains(tmp.path().to_str().unwrap()));
        }
    }

    #[test]
    fn test_format_agents_for_planner() {
        let catalog = Catalog::new();
        let formatted_no_cfg = catalog.format_agents_for_planner(None);
        assert!(formatted_no_cfg.contains("## Available Agent Archetypes & Tool Capabilities"));
        assert!(formatted_no_cfg.contains("- **`coder`**"));
        assert!(formatted_no_cfg.contains("- Built-in tools:"));
        assert!(formatted_no_cfg.contains("`read_file`"));
        assert!(formatted_no_cfg.contains("`write_file`"));
        // Checkpoint/control tools must be filtered out
        assert!(!formatted_no_cfg.contains(TOOL_REBIRTH));
        assert!(!formatted_no_cfg.contains(TOOL_REPLY_TO_ARBITRATOR));
        // No MCP tools without active cfg or registered servers
        assert!(!formatted_no_cfg.contains("Allowed MCP tools:"));

        let cfg = crate::config::Config::default();
        let formatted_default_cfg = catalog.format_agents_for_planner(Some(&cfg));
        assert_eq!(formatted_no_cfg, formatted_default_cfg);
    }
}
