//! Marmennill (marmel) — clean-room agentic coding assistant library crate.

/// Manager-level core: turn loop state machine, plan management, and context engine.
pub mod manager;
/// Backwards-compatibility alias for the manager module.
pub use manager as agent;
/// Specialist subagents (Coder, Debugger, Researcher, Generalist, Validator), live runner, and automated verification.
pub mod agents;
pub mod config;
pub mod debug_log;
pub mod harness;
pub mod llm;
pub mod mcp;
pub mod orchestrator;
pub mod prompts;
pub mod tool_names;
pub mod types;
pub mod ui;
