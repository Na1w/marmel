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
}
