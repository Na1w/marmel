//! Statically embedded prompts (compiled in at build time via `include_str!`).

pub const SYSTEM_PROMPT: &str = include_str!("../prompts/system.md");
pub const STEER_ARBITRATOR_PROMPT: &str = include_str!("../prompts/steer_arbitrator.md");
pub const CODER_PROMPT: &str = include_str!("../prompts/coder.md");
pub const DEBUGGER_PROMPT: &str = include_str!("../prompts/debugger.md");
pub const RESEARCHER_PROMPT: &str = include_str!("../prompts/researcher.md");
pub const GENERALIST_PROMPT: &str = include_str!("../prompts/generalist.md");
pub const VALIDATOR_PROMPT: &str = include_str!("../prompts/validator.md");
pub const VALIDATOR_CODER_PROMPT: &str = include_str!("../prompts/validator_coder.md");
pub const VALIDATOR_DEBUGGER_PROMPT: &str = include_str!("../prompts/validator_debugger.md");
pub const VALIDATOR_RESEARCHER_PROMPT: &str = include_str!("../prompts/validator_researcher.md");
pub const VALIDATOR_GENERALIST_PROMPT: &str = include_str!("../prompts/validator_generalist.md");
pub const VALIDATOR_PLANNER_PROMPT: &str = include_str!("../prompts/validator_planner.md");
pub const PLANNER_PROMPT: &str = include_str!("../prompts/planner.md");
pub const PROMPT_BUILDER_PROMPT: &str = include_str!("../prompts/prompt_builder.md");

pub const SKILL_CLEAN_CODE: &str = include_str!("../prompts/skills/clean_code.md");
pub const SKILL_DEBUGGING: &str = include_str!("../prompts/skills/debugging.md");
pub const SKILL_RESEARCH: &str = include_str!("../prompts/skills/research.md");
pub const SKILL_VERIFICATION: &str = include_str!("../prompts/skills/verification.md");
pub const SKILL_TESTING: &str = include_str!("../prompts/skills/testing.md");

/// Returns a markdown section describing the current operating system, shell,
/// architecture, and workspace directory for injection into agent prompts.
pub fn format_environment_block() -> String {
    let os = std::env::consts::OS;
    let arch = std::env::consts::ARCH;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| {
        if cfg!(windows) {
            "powershell/cmd".to_string()
        } else {
            "bash/sh".to_string()
        }
    });
    let cwd = std::env::current_dir().map_or_else(|_| ".".to_string(), |p| p.display().to_string());
    format!(
        "## Workspace & Environment\n- Operating System: `{os}` ({arch})\n- Shell: `{shell}`\n- Current Working Directory (CWD): `{cwd}`\n- All relative file paths, workspace tool executions, commands, and search operations resolve against this workspace directory."
    )
}

/// Format a markdown section containing configured MCP server instructions and
/// advertised MCP tool guidance for the given `mcp_servers` allowlist.
///
/// Only servers listed in `mcp_servers` (i.e. servers the target agent is allowed
/// to use) are included. Returns an empty string when `mcp_servers` is empty or
/// when no matching server has instructions or live tools.
pub fn format_mcp_prompt_block(cfg: &crate::config::Config, mcp_servers: &[String]) -> String {
    if mcp_servers.is_empty() {
        return String::new();
    }
    let mut sections = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for server_name in mcp_servers {
        if !seen.insert(server_name.as_str()) {
            continue;
        }
        let server_tools = crate::harness::allowed_mcp_tools(std::slice::from_ref(server_name));
        let custom_prompt = cfg
            .mcp_servers
            .get(server_name)
            .and_then(|sc| sc.prompt.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty());

        if server_tools.is_empty() && custom_prompt.is_none() {
            continue;
        }

        let mut block = format!("### MCP Server: `{server_name}`\n");
        if !server_tools.is_empty() {
            let tool_names: Vec<String> = server_tools
                .iter()
                .map(|t| format!("`{}`", t.qualified_name()))
                .collect();
            block.push_str(&format!(
                "- Available MCP tools: {}\n",
                tool_names.join(", ")
            ));
        }
        if let Some(prompt_text) = custom_prompt {
            block.push_str(prompt_text);
            if !prompt_text.ends_with('\n') {
                block.push('\n');
            }
        }
        sections.push(block);
    }
    if sections.is_empty() {
        String::new()
    } else {
        format!("\n## MCP Server Guidance\n{}", sections.join("\n"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_format_environment_block_contains_os_and_arch() {
        let block = format_environment_block();
        assert!(block.contains("## Workspace & Environment"));
        assert!(block.contains(std::env::consts::OS));
        assert!(block.contains(std::env::consts::ARCH));
        assert!(block.contains("Current Working Directory (CWD):"));
    }

    #[test]
    fn test_format_mcp_prompt_block_injects_only_allowed_servers() {
        let mut cfg = crate::config::Config::default();
        cfg.mcp_servers.insert(
            "kiwix".to_string(),
            crate::mcp::McpServerConfig {
                command: Some("zim-search".to_string()),
                prompt: Some("Use kiwix__search first for offline research.".to_string()),
                ..Default::default()
            },
        );
        cfg.mcp_servers.insert(
            "other".to_string(),
            crate::mcp::McpServerConfig {
                command: Some("other-mcp".to_string()),
                prompt: Some("Other server instructions.".to_string()),
                ..Default::default()
            },
        );

        let empty = format_mcp_prompt_block(&cfg, &[]);
        assert!(empty.is_empty());

        let researcher_block = format_mcp_prompt_block(&cfg, &["kiwix".to_string()]);
        assert!(researcher_block.contains("## MCP Server Guidance"));
        assert!(researcher_block.contains("### MCP Server: `kiwix`"));
        assert!(researcher_block.contains("Use kiwix__search first for offline research."));
        assert!(!researcher_block.contains("Other server instructions."));
    }
}
