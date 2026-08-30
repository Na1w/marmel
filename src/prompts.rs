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
