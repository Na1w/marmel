//! Marmennill (marmel) — clean-room agentic coding assistant library crate.

/// Specialist subagents (Coder, Debugger, Researcher, Generalist, Validator), live runner, and automated verification.
pub mod agents;
pub mod config;
pub mod debug_log;
pub mod harness;
pub mod llm;
/// Manager-level core: turn loop state machine, plan management, and context engine.
pub mod manager;
pub mod mcp;
/// Shared network plumbing (SSE pump skeleton, retry/backoff, HTTP client builder).
pub mod net;
pub mod orchestrator;
pub mod prompts;
pub mod tool_names;
pub mod types;
pub mod ui;
