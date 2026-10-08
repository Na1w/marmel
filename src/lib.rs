//! Marmennill (marmel) — clean-room agentic coding assistant library crate.

/// Specialist subagents (Coder, Debugger, Researcher, Generalist, Validator), live runner, and automated verification.
pub mod agents;
pub mod config;
pub mod debug_log;
pub mod harness;
pub mod llm;
/// Manager-level core: turn loop state machine, plan management, and context engine.
pub mod manager;
/// Single owner of the mission-marker grammar (`MISSION COMPLETE` / `FAILED` /
/// `REPLAN REQUIRED`): the marker set, its precedence rules, and the parser used
/// by both the specialist (agents) and plan (manager) layers.
pub mod markers;
pub mod mcp;
/// Shared network plumbing (SSE pump skeleton, retry/backoff, HTTP client builder).
pub mod net;
pub mod orchestrator;
/// Single owner of the execution-plan task-line grammar (`- [ ] [t-001] …`,
/// `- [ ] (t-002) …`, `* [x] [t-003] …` …): the checkbox/task-id parser, the
/// check-off line rewriter and the task-id token recogniser shared by the
/// manager (on-disk authority), orchestrator, agents and UI layers.
pub mod plan_parse;
pub mod prompts;
/// Canonical task-identifier normalization shared by the agents, harness and manager layers.
pub mod task_id;
/// Char-boundary-safe truncation/ellipsis helpers shared across layers.
pub mod text_util;
/// Single owner of the one-line human preview of a tool call's JSON arguments:
/// the per-tool argument keys (with tolerated legacy aliases) and the
/// char-boundary-safe clipping shared by the specialist runner and the UI.
pub mod tool_args;
pub mod tool_names;
pub mod types;
pub mod ui;
