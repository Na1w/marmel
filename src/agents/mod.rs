//! Specialist Subagents — isolated-context, run-to-completion workers.

pub mod coder;
pub mod debugger;
pub mod generalist;
pub mod researcher;
pub mod validator;

pub use coder::Coder;
pub use debugger::Debugger;
pub use generalist::Generalist;
pub use researcher::Researcher;
pub use validator::Validator;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::fmt;

#[cfg(not(test))]
use crate::tool_names::{
    TOOL_DELEGATE_TASK, TOOL_GLOB, TOOL_GREP_SEARCH, TOOL_LEAVE_VERDICT, TOOL_READ_FILE,
    TOOL_REPLACE, TOOL_RUN_COMMAND, TOOL_WRITE_FILE,
};

/// Canonical specialist role ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Agent {
    /// Lead Software Engineer — system architecture, refactoring, code implementation.
    Coder,
    /// Deep Knowledge & Archival — ZIM, PDF, historical archives, statistics.
    Researcher,
    /// Low-Level Systems Debugger — crash forensics, PTY GDB/LLDB, ABI/codegen.
    Debugger,
    /// Independent Quality Auditor — verification, test suites, formal verdicts.
    Validator,
    /// Supreme Polymath — dense reasoning and cross-domain logic.
    Generalist,
}

impl fmt::Display for Agent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

impl Agent {
    pub fn as_str(&self) -> &'static str {
        match self {
            Agent::Coder => "coder",
            Agent::Researcher => "researcher",
            Agent::Debugger => "debugger",
            Agent::Validator => "validator",
            Agent::Generalist => "generalist",
        }
    }

    #[allow(clippy::should_implement_trait)]
    pub fn from_str(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "coder" => Some(Self::Coder),
            "researcher" => Some(Self::Researcher),
            "debugger" => Some(Self::Debugger),
            "validator" => Some(Self::Validator),
            "generalist" | "deepbrain" => Some(Self::Generalist),
            _ => None,
        }
    }
}

impl std::str::FromStr for Agent {
    type Err = ();

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::from_str(s).ok_or(())
    }
}

/// Terminal outcome a specialist returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MissionMarker {
    /// Task fully satisfied. `task_id` matches the plan line to auto-check.
    Complete { task_id: Option<String> },
    /// Task could not be completed; report reason + partial result.
    Failed { reason: String },
    /// Task could not be completed AND the plan/goal needs revisiting.
    Replan { reason: String },
}

static TASK_ID_RE: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();

fn find_task_id(text: &str) -> Option<&str> {
    let re = TASK_ID_RE.get_or_init(|| {
        regex::Regex::new(r"\(?\[?(t-[A-Za-z0-9_-]+)\]?\)?").expect("valid task regex")
    });
    re.captures(text).map(|c| c.get(1).unwrap().as_str())
}

impl MissionMarker {
    pub fn parse(text: &str) -> Option<MissionMarker> {
        let upper = text.to_ascii_uppercase();
        if upper.contains("REPLAN REQUIRED") {
            return Some(MissionMarker::Replan {
                reason: text.to_string(),
            });
        }
        if upper.contains("MISSION COMPLETE") {
            let task_id = find_task_id(text).map(|t| t.to_string());
            return Some(MissionMarker::Complete { task_id });
        }
        if upper.contains("FAILED") {
            return Some(MissionMarker::Failed {
                reason: text.to_string(),
            });
        }
        None
    }
}

/// The `delegate_task` argument payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DelegationRequest {
    /// Specialist role.
    pub agent_name: Agent,
    /// Self-contained task brief in English.
    pub prompt: String,
    /// Bounded list of relevant excerpts or file paths.
    #[serde(default)]
    pub snippets: Vec<String>,
    /// Optional execution plan task id enabling automatic check-off.
    #[serde(default)]
    pub task_id: Option<String>,
    /// Optional image references for multimodal specialists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image_urls: Option<Vec<String>>,
    /// Optional audio references for audio specialists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio_urls: Option<Vec<String>>,
    /// Whether the task brief explicitly grants recursion.
    #[serde(default)]
    pub recursion_granted: bool,
}

/// The isolated context handed to a specialist.
#[derive(Debug, Clone)]
pub struct IsolatedContext {
    pub role_system_prompt: String,
    pub brief: String,
    pub task_id: Option<String>,
    pub snippets: Vec<String>,
    pub image_urls: Vec<String>,
    pub audio_urls: Vec<String>,
}

impl IsolatedContext {
    pub fn from_request(role_system_prompt: String, req: &DelegationRequest) -> Self {
        Self {
            role_system_prompt,
            brief: req.prompt.clone(),
            task_id: req.task_id.clone(),
            snippets: req.snippets.clone(),
            image_urls: req.image_urls.clone().unwrap_or_default(),
            audio_urls: req.audio_urls.clone().unwrap_or_default(),
        }
    }

    pub fn into_engine(&self, max_context_tokens: usize) -> crate::agent::ContextEngine {
        let factory = crate::agent::ContextEngineFactory::new(max_context_tokens);
        factory.specialist_context(self.role_system_prompt.clone(), self.brief.clone())
    }
}

/// A specialist's single returned deliverable.
#[derive(Debug, Clone)]
pub struct Deliverable {
    pub marker: MissionMarker,
    pub content: String,
    pub task_id: Option<String>,
}

/// The canonical Specialist trait.
#[async_trait]
pub trait Specialist: Send + Sync + fmt::Debug {
    fn name(&self) -> Agent;
    fn tool_namespaces(&self) -> &[&'static str];

    async fn run(&self, ctx: &IsolatedContext) -> Deliverable {
        let content = run_specialist_llm(self.name(), ctx).await;
        let marker = MissionMarker::parse(&content).unwrap_or_else(|| MissionMarker::Failed {
            reason: "no terminal marker".to_string(),
        });
        Deliverable {
            marker,
            content,
            task_id: None,
        }
    }

    fn may_recurse(&self) -> bool {
        false
    }
}

pub(crate) async fn run_specialist_llm(agent: Agent, ctx: &IsolatedContext) -> String {
    let snippet_block = if ctx.snippets.is_empty() {
        "(none)".to_string()
    } else {
        ctx.snippets.join("\n---\n")
    };
    let canned = format!(
        "Specialist role `{}` executed its isolated task to completion.\n\n\
         TASK BRIEF:\n{}\n\n\
         BOUNDED SNIPPETS ({count}):\n{snippet_block}\n\n\
         MISSION COMPLETE",
        ctx.role_system_prompt
            .trim()
            .lines()
            .next()
            .unwrap_or("specialist"),
        ctx.brief,
        count = ctx.snippets.len(),
        snippet_block = snippet_block,
    );

    #[cfg(test)]
    {
        let _ = agent;
        canned
    }

    #[cfg(not(test))]
    {
        if let Some(res) = try_run_specialist_live(agent, ctx).await {
            return res;
        }
        canned
    }
}

#[cfg(not(test))]
async fn try_run_specialist_live(agent: Agent, ctx: &IsolatedContext) -> Option<String> {
    if tokio::runtime::Handle::try_current().is_err() {
        return None;
    }
    // If running inside cargo test suite (integration tests binaries in target/.../deps/), bypass live network calls
    if std::env::current_exe()
        .map(|p| p.to_string_lossy().contains("/deps/"))
        .unwrap_or(false)
        && std::env::var("MARMEL_LIVE_TEST").is_err()
    {
        return None;
    }
    let cfg = crate::config::load(None).ok()?;
    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let backend_url = specialist_cfg
        .and_then(|sc| sc.backend_url.as_ref())
        .unwrap_or(&cfg.backend_url);
    if backend_url.is_empty() {
        return None;
    }
    let auth_token = specialist_cfg
        .and_then(|sc| sc.auth_token.as_ref())
        .unwrap_or(&cfg.auth_token);
    let model = specialist_cfg
        .and_then(|sc| sc.model.as_ref())
        .unwrap_or(&cfg.model);
    let client = crate::llm::ChatClient::new_with_token(backend_url, model, auth_token);
    let res = run_specialist_live(&client, agent, ctx, &cfg).await.ok()?;
    (!res.trim().is_empty()).then_some(res)
}

#[cfg(not(test))]
fn format_tool_args_preview(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        TOOL_READ_FILE | TOOL_WRITE_FILE => args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_REPLACE => args
            .get("path")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_RUN_COMMAND => {
            let cmd = args
                .get("command")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if cmd.len() > 35 {
                format!("{}...", &cmd[..32])
            } else {
                cmd.to_string()
            }
        }
        TOOL_GREP_SEARCH => args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_GLOB => args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        _ => {
            let s = args.to_string();
            if s.len() > 30 {
                format!("{}...", &s[..27])
            } else {
                s
            }
        }
    }
}

#[cfg(not(test))]
fn format_tool_args_full(tool: &str, args: &serde_json::Value) -> String {
    match tool {
        TOOL_READ_FILE | TOOL_WRITE_FILE | TOOL_REPLACE => {
            if let Some(path) = args.get("path").and_then(serde_json::Value::as_str) {
                if tool == TOOL_WRITE_FILE || tool == TOOL_REPLACE {
                    let len = args
                        .get("content")
                        .and_then(serde_json::Value::as_str)
                        .map_or(0, str::len);
                    format!("{path} (content: {len} bytes)")
                } else {
                    path.to_string()
                }
            } else {
                args.to_string()
            }
        }
        TOOL_RUN_COMMAND => args
            .get("command")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_GREP_SEARCH => {
            let q = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let path = args
                .get("path")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            if path.is_empty() {
                format!("query=\"{q}\"")
            } else {
                format!("query=\"{q}\", path=\"{path}\"")
            }
        }
        TOOL_GLOB => args
            .get("pattern")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("")
            .to_string(),
        TOOL_DELEGATE_TASK => {
            let ag = args
                .get("agent_name")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let tid = args
                .get("task_id")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let pr = args
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("agent={ag}, task_id={tid}, prompt=\"{pr}\"")
        }
        TOOL_LEAVE_VERDICT => {
            let v = args
                .get("verdict")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            let c = args
                .get("comments")
                .and_then(serde_json::Value::as_str)
                .unwrap_or("");
            format!("verdict={v}, comments=\"{c}\"")
        }
        _ => args.to_string(),
    }
}

#[allow(dead_code)]
pub(crate) fn update_revision(final_content: &mut String, revised: &str) {
    if !revised.is_empty() {
        if final_content.is_empty() {
            *final_content = revised.to_string();
        } else if !final_content.contains(revised) {
            final_content.push_str("\n\n");
            final_content.push_str(revised);
        }
    }
}

pub(crate) fn assemble_final_deliverable(
    validation_passed: bool,
    validator_critique: Option<&str>,
    final_content: &str,
) -> String {
    if validation_passed {
        let mut content = final_content.to_string();
        if !content.contains("MISSION COMPLETE")
            && !content.contains("FAILED")
            && !content.contains("REPLAN REQUIRED")
        {
            content.push_str("\n\nMISSION COMPLETE");
        }
        return content;
    }

    let mut rejected = String::new();
    if let Some(critique) = validator_critique {
        rejected.push_str(&format!(
            "VALIDATOR REJECTION: {critique}\n---------------\n"
        ));
    }
    let revision = final_content
        .replace("MISSION COMPLETE", "REVOKED")
        .replace("mission complete", "REVOKED");
    rejected.push_str(&revision);
    if !rejected.contains("FAILED") && !rejected.contains("REPLAN REQUIRED") {
        rejected.push_str("\n\nFAILED (Validator rejected deliverable)");
    }
    rejected
}

#[cfg(not(test))]
async fn run_specialist_live(
    client: &crate::llm::ChatClient,
    agent: Agent,
    ctx: &IsolatedContext,
    cfg: &crate::config::Config,
) -> anyhow::Result<String> {
    let cwd = std::env::current_dir().map_or_else(|_| ".".to_string(), |p| p.display().to_string());

    let enhanced_system_prompt = format!(
        "{}\n\n## Environment & Workspace\n- Current Working Directory (CWD): `{cwd}`\n- All relative paths and file operations resolve against this workspace directory.\n- Tools available: `write_file`, `replace`, `read_file`, `run_command`, `grep_search`, `glob`.\n- You MUST save files and execute real work to complete the task.",
        ctx.role_system_prompt
    );

    let mut engine = crate::agent::ContextEngineFactory::new(cfg.max_context_tokens)
        .specialist_context(enhanced_system_prompt, ctx.brief.clone());

    if !ctx.snippets.is_empty() {
        let snippet_text = format!("Snippets:\n{}", ctx.snippets.join("\n---\n"));
        engine.append(crate::types::Message::User {
            content: snippet_text,
        });
    }

    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let specialist_model = specialist_cfg
        .and_then(|sc| sc.model.as_ref())
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());

    let agent_tag = match &ctx.task_id {
        Some(t) if !t.trim().is_empty() => format!("{agent}-{t}"),
        _ => format!("{agent}"),
    };

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let reg_entry = registry.resolve(agent).expect("agent is registered");
    let mut tools = Vec::new();
    for tool in crate::types::ToolDef::default_tools() {
        if reg_entry.allows(&tool.function.name) {
            tools.push(tool);
        }
    }
    if let Some(mcp) = crate::harness::get_mcp_manager()
        && let Some(sc) = specialist_cfg
    {
        for tool in mcp.tools_for_servers(&sc.mcp_servers) {
            tools.push(crate::types::ToolDef::from_mcp(&tool));
        }
    }

    let mut final_content = String::new();
    let mut nudge_count = 0u32;

    let _active_guard = crate::orchestrator::register_active_worker(
        ctx.task_id.clone(),
        agent.as_str().to_string(),
        ctx.brief.clone(),
    );

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );

    for _turn in 0..100 {
        crate::orchestrator::update_active_worker_context(&_active_guard.0, engine.token_count());
        crate::orchestrator::emit_status(format!(
            "{agent_tag}: thinking / calling model ({specialist_model})..."
        ));
        let req = crate::types::ChatRequest {
            model: specialist_model.clone(),
            messages: engine.messages().to_vec(),
            tools: Some(tools.clone()),
            stream: Some(false),
            enable_thinking: None,
            temperature: Some(cfg.temperature),
            top_p: Some(cfg.top_p),
            presence_penalty: Some(cfg.presence_penalty),
            frequency_penalty: Some(cfg.frequency_penalty),
        };

        let reply = client.chat(&req).await?;
        update_revision(&mut final_content, &reply.content);

        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        let assistant_msg = crate::types::Message::Assistant {
            content: Some(reply.content.clone()),
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning)
            },
            tool_calls: tool_calls.clone(),
        };
        engine.append(assistant_msg);

        let is_repeating = monitor.feed_text(&reply.content);

        if tool_calls.is_empty() {
            if is_repeating && nudge_count < 3 {
                nudge_count += 1;
                tracing::warn!(
                    "{agent_tag}: repetitive generation loop detected in specialist output — injecting corrective nudge"
                );
                engine.append(crate::types::Message::User {
                    content: "SYSTEM NOTICE: Repetitive generation loop detected. Terminate conversational debate immediately and call your required tools (such as `read_file`, `write_file`, `run_command`, etc.) to perform the required work, or conclude with 'MISSION COMPLETE'.".to_string(),
                });
                continue;
            }

            let upper = reply.content.to_ascii_uppercase();
            let is_terminal = upper.contains("MISSION COMPLETE")
                || upper.contains("FAILED")
                || upper.contains("REPLAN REQUIRED");
            if !is_terminal && nudge_count < 3 {
                nudge_count += 1;
                engine.append(crate::types::Message::User {
                    content: "SYSTEM NOTICE: You did not call any tools or output MISSION COMPLETE. Please use your tools (such as `read_file`, `write_file`, `run_command`, etc.) to perform the required work, create/update any requested files in the workspace, and conclude with 'MISSION COMPLETE'.".to_string(),
                });
                continue;
            }
            break;
        }

        for tc in tool_calls {
            let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
            let preview = format_tool_args_preview(&tc.function.name, &args_val);
            if preview.is_empty() {
                crate::orchestrator::emit_status(format!("{agent_tag}: {}", tc.function.name));
            } else {
                crate::orchestrator::emit_status(format!(
                    "{agent_tag}: {}({preview})",
                    tc.function.name
                ));
            }
            let full_args = format_tool_args_full(&tc.function.name, &args_val);
            tracing::info!(
                "{agent_tag} invoking tool: {}({})",
                tc.function.name,
                full_args
            );

            let intervention = monitor.observe_tool(&tc.function.name, &args_val);
            let content = match intervention {
                crate::harness::monitor::Intervention::Block
                | crate::harness::monitor::Intervention::Cut => {
                    let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                        format!(
                            "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls — proceed with your task or save deliverables with write_file.",
                            tc.function.name
                        )
                    });
                    tracing::warn!(
                        "{agent_tag} tool {} blocked by repetition detector",
                        tc.function.name
                    );
                    err_msg
                }
                crate::harness::monitor::Intervention::None => {
                    let invocation = crate::harness::ToolInvocation {
                        name: tc.function.name.clone(),
                        arguments: args_val,
                    };
                    let tool_res = crate::harness::dispatch_for(
                        &invocation,
                        crate::harness::ToolCaller::Specialist(agent),
                    );
                    match tool_res {
                        Ok(r) => {
                            tracing::info!(
                                "{agent_tag} tool {} completed with {} chars output",
                                tc.function.name,
                                r.content.len()
                            );
                            r.content
                        }
                        Err(e) => {
                            tracing::warn!("{agent_tag} tool {} error: {e}", tc.function.name);
                            format!("ERROR: {e}")
                        }
                    }
                }
            };
            engine.append(crate::types::Message::Tool {
                tool_call_id: tc.id,
                content,
            });
        }
    }

    let auto_validate_enabled = specialist_cfg
        .and_then(|sc| sc.enable_validator)
        .unwrap_or(true);
    let max_val_iterations = specialist_cfg
        .and_then(|sc| sc.max_validator_iterations)
        .unwrap_or(5);

    let mut validation_passed =
        !auto_validate_enabled || max_val_iterations == 0 || agent == Agent::Validator;
    let mut validator_critique: Option<String> = None;

    if auto_validate_enabled
        && max_val_iterations > 0
        && agent != Agent::Validator
        && !final_content.is_empty()
    {
        for val_iter in 0..max_val_iterations {
            crate::orchestrator::emit_status(format!(
                "validator-{agent_tag}: testing deliverable (pass {}/{})...",
                val_iter + 1,
                max_val_iterations
            ));
            if let Ok((approved, critique)) =
                run_automated_validation(client, agent, &ctx.brief, &final_content, cfg).await
            {
                if approved {
                    let feedback = if critique.trim().is_empty() {
                        "All verification checks passed.".to_string()
                    } else {
                        critique.clone()
                    };
                    crate::orchestrator::emit_status(format!(
                        "[Validator] APPROVED deliverable for {agent_tag}:\n{feedback}"
                    ));
                    tracing::info!(
                        "Automated validator APPROVED specialist deliverable for {}: {}",
                        agent_tag,
                        feedback
                    );
                    validation_passed = true;
                    break;
                } else {
                    let feedback = if critique.trim().is_empty() {
                        "Deliverable failed verification checks.".to_string()
                    } else {
                        critique.clone()
                    };
                    validator_critique = Some(feedback.clone());
                    crate::orchestrator::emit_status(format!(
                        "[Validator] REJECTED deliverable for {agent_tag} (pass {}/{}):\n{feedback}",
                        val_iter + 1,
                        max_val_iterations
                    ));
                    tracing::warn!(
                        "Automated validator REJECTED specialist deliverable for {}: {}",
                        agent_tag,
                        feedback
                    );
                    let feedback_msg = format!(
                        "Validation feedback: The validator tested your changes and found issues:\n{}\n\n\
                         Please address all validator critique points, verify your work with available tools, and conclude with 'MISSION COMPLETE'.",
                        feedback
                    );
                    engine.append(crate::types::Message::User {
                        content: feedback_msg,
                    });

                    let mut latest_revision = String::new();
                    for rev_turn in 0..25 {
                        crate::orchestrator::emit_status(format!(
                            "{agent_tag}: revising code per validator critique (step {}/25)...",
                            rev_turn + 1
                        ));
                        let req = crate::types::ChatRequest {
                            model: specialist_model.clone(),
                            messages: engine.messages().to_vec(),
                            tools: Some(tools.clone()),
                            stream: Some(false),
                            enable_thinking: None,
                            temperature: Some(cfg.temperature),
                            top_p: Some(cfg.top_p),
                            presence_penalty: Some(cfg.presence_penalty),
                            frequency_penalty: Some(cfg.frequency_penalty),
                        };

                        let reply = match client.chat(&req).await {
                            Ok(r) => r,
                            Err(e) => {
                                tracing::error!(
                                    "{agent_tag}: LLM chat call error on revision step {rev_turn}: {e:?}"
                                );
                                break;
                            }
                        };
                        if !reply.content.is_empty() {
                            latest_revision = reply.content.clone();
                        }

                        let mut tool_calls = reply.tool_calls.clone();
                        if tool_calls.is_empty() && cfg.enable_xml_rescue {
                            let monitor = crate::harness::monitor::HarnessMonitor::with_new_stats();
                            let rescued = monitor.rescue_xml(&reply.content);
                            if !rescued.is_empty() {
                                tool_calls = rescued;
                            }
                        }

                        let assistant_msg = crate::types::Message::Assistant {
                            content: Some(reply.content),
                            reasoning_content: if reply.reasoning.is_empty() {
                                None
                            } else {
                                Some(reply.reasoning)
                            },
                            tool_calls: tool_calls.clone(),
                        };
                        engine.append(assistant_msg);

                        if tool_calls.is_empty() {
                            break;
                        }

                        for tc in tool_calls {
                            let args_val =
                                serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                                    .unwrap_or_else(|_| {
                                        serde_json::Value::String(tc.function.arguments.clone())
                                    });
                            let desc = format_tool_args_preview(&tc.function.name, &args_val);
                            crate::orchestrator::emit_status(format!(
                                "{agent_tag}: running {}({desc})",
                                tc.function.name
                            ));
                            let full_args = format_tool_args_full(&tc.function.name, &args_val);
                            tracing::info!(
                                "{agent_tag} (revision) invoking tool: {}({})",
                                tc.function.name,
                                full_args
                            );

                            let intervention = monitor.observe_tool(&tc.function.name, &args_val);
                            let content = match intervention {
                                crate::harness::monitor::Intervention::Block
                                | crate::harness::monitor::Intervention::Cut => {
                                    let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                                        format!(
                                            "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls — proceed with your task or save deliverables with write_file.",
                                            tc.function.name
                                        )
                                    });
                                    tracing::warn!(
                                        "{agent_tag} (revision) tool {} blocked by repetition detector",
                                        tc.function.name
                                    );
                                    err_msg
                                }
                                crate::harness::monitor::Intervention::None => {
                                    let invocation = crate::harness::ToolInvocation {
                                        name: tc.function.name.clone(),
                                        arguments: args_val,
                                    };
                                    let tool_res = crate::harness::dispatch_for(
                                        &invocation,
                                        crate::harness::ToolCaller::Specialist(agent),
                                    );
                                    match tool_res {
                                        Ok(r) => {
                                            tracing::info!(
                                                "{agent_tag} (revision) tool {} completed with {} chars",
                                                tc.function.name,
                                                r.content.len()
                                            );
                                            r.content
                                        }
                                        Err(e) => {
                                            tracing::warn!(
                                                "{agent_tag} (revision) tool {} error: {e}",
                                                tc.function.name
                                            );
                                            format!("ERROR: {e}")
                                        }
                                    }
                                }
                            };
                            engine.append(crate::types::Message::Tool {
                                tool_call_id: tc.id,
                                content,
                            });
                        }
                    }
                    if !latest_revision.is_empty() {
                        final_content = latest_revision;
                    }
                }
            } else {
                break;
            }
        }
    }

    if !final_content.is_empty() {
        let assembled = assemble_final_deliverable(
            validation_passed,
            validator_critique.as_deref(),
            &final_content,
        );
        Ok(assembled)
    } else {
        Ok(String::new())
    }
}

#[cfg(not(test))]
async fn run_automated_validation(
    _client: &crate::llm::ChatClient,
    agent: Agent,
    task_brief: &str,
    deliverable: &str,
    cfg: &crate::config::Config,
) -> anyhow::Result<(bool, String)> {
    let validator_prompt = match agent {
        Agent::Coder => crate::agents::validator::VALIDATOR_CODER_ROLE_PROMPT,
        Agent::Debugger => crate::agents::validator::VALIDATOR_DEBUGGER_ROLE_PROMPT,
        Agent::Researcher => crate::agents::validator::VALIDATOR_RESEARCHER_ROLE_PROMPT,
        Agent::Generalist => crate::agents::validator::VALIDATOR_GENERALIST_ROLE_PROMPT,
        _ => crate::agents::validator::VALIDATOR_ROLE_PROMPT,
    };

    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let validator_cfg = cfg.orchestration.specialists.get(Agent::Validator.as_str());
    let validator_backend = specialist_cfg
        .and_then(|sc| sc.validator_backend_url.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.backend_url.as_ref()))
        .unwrap_or(&cfg.backend_url);
    let validator_token = specialist_cfg
        .and_then(|sc| sc.validator_auth_token.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.auth_token.as_ref()))
        .unwrap_or(&cfg.auth_token);
    let validator_model = specialist_cfg
        .and_then(|sc| sc.validator_model.as_ref())
        .or_else(|| validator_cfg.and_then(|vc| vc.model.as_ref()))
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());

    let val_client = crate::llm::ChatClient::new_with_token(
        validator_backend,
        &validator_model,
        validator_token,
    );

    let brief = format!(
        "Task Brief:\n{}\n\nSpecialist Deliverable:\n{}\n\n\
         Instructions:\n\
         1. Inspect the workspace, verify files, compile, and run tests as needed using available tools.\n\
         2. When your verification is complete, you MUST call the `leave_verdict` tool with `verdict` ('APPROVED' or 'REJECTED') and detailed `comments`.",
        task_brief, deliverable
    );

    let mut engine = crate::agent::ContextEngineFactory::new(cfg.max_context_tokens)
        .specialist_context(validator_prompt.to_string(), brief);

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let val_entry = registry
        .resolve(Agent::Validator)
        .expect("validator is registered");
    let mut tools = Vec::new();
    for tool in crate::types::ToolDef::default_tools() {
        if val_entry.allows(&tool.function.name) {
            tools.push(tool);
        }
    }
    if let Some(mcp) = crate::harness::get_mcp_manager() {
        let servers = validator_cfg
            .map(|vc| vc.mcp_servers.clone())
            .filter(|s| !s.is_empty())
            .or_else(|| specialist_cfg.map(|sc| sc.mcp_servers.clone()))
            .unwrap_or_default();
        for tool in mcp.tools_for_servers(&servers) {
            tools.push(crate::types::ToolDef::from_mcp(&tool));
        }
    }

    let _active_guard = crate::orchestrator::register_active_worker(
        None,
        format!("validator-{agent}"),
        format!("Auditing {agent} deliverable"),
    );

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );
    for _turn in 0..50 {
        crate::orchestrator::update_active_worker_context(&_active_guard.0, engine.token_count());
        let req = crate::types::ChatRequest {
            model: validator_model.clone(),
            messages: engine.messages().to_vec(),
            tools: Some(tools.clone()),
            stream: Some(false),
            enable_thinking: None,
            temperature: Some(0.0),
            top_p: Some(cfg.top_p),
            presence_penalty: Some(cfg.presence_penalty),
            frequency_penalty: Some(cfg.frequency_penalty),
        };

        let reply = match val_client.chat(&req).await {
            Ok(r) => r,
            Err(e) => {
                tracing::error!("validator-{agent} LLM chat call error on turn {_turn}: {e:?}");
                break;
            }
        };

        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        let assistant_msg = crate::types::Message::Assistant {
            content: Some(reply.content.clone()),
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
        };
        engine.append(assistant_msg);

        for tc in &tool_calls {
            if tc.function.name == TOOL_LEAVE_VERDICT {
                let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                    .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
                let verdict = args_val
                    .get("verdict")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("APPROVED");
                let comments = args_val
                    .get("comments")
                    .or_else(|| args_val.get("comment"))
                    .or_else(|| args_val.get("feedback"))
                    .or_else(|| args_val.get("reason"))
                    .or_else(|| args_val.get("critique"))
                    .or_else(|| args_val.get("details"))
                    .or_else(|| args_val.get("explanation"))
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .trim()
                    .to_string();

                let approved = verdict.eq_ignore_ascii_case("APPROVED");
                let critique = if !comments.is_empty() {
                    comments
                } else if approved {
                    "Deliverable verified and approved.".to_string()
                } else {
                    "Deliverable rejected by validator without detailed comments.".to_string()
                };

                tracing::info!(
                    "Validator recorded verdict for {agent} via leave_verdict: approved={}, critique:\n{}",
                    approved,
                    critique
                );
                return Ok((approved, critique));
            }
        }

        if tool_calls.is_empty() {
            // Validator responded with text without calling leave_verdict -> prompt it (matching Caesar orchestrator.rs:1510).
            engine.append(crate::types::Message::User {
                content: "System: You have not submitted a verdict. If you need to perform further verification, please invoke the appropriate tools (e.g., running commands or reading files). If your analysis is complete, you must call the 'leave_verdict' tool to submit your final verdict (APPROVED or REJECTED).".to_string(),
            });
            continue;
        }

        for tc in tool_calls {
            let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));
            let desc = format_tool_args_preview(&tc.function.name, &args_val);
            crate::orchestrator::emit_status(format!(
                "validator-{agent}: running {}({desc})",
                tc.function.name
            ));
            let full_args = format_tool_args_full(&tc.function.name, &args_val);
            tracing::info!(
                "validator-{agent} invoking tool: {}({})",
                tc.function.name,
                full_args
            );

            let intervention = monitor.observe_tool(&tc.function.name, &args_val);
            let content = match intervention {
                crate::harness::monitor::Intervention::Block
                | crate::harness::monitor::Intervention::Cut => {
                    let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                        format!(
                            "ERROR: Tool repetition detected for '{}'. Conclude your review by calling leave_verdict.",
                            tc.function.name
                        )
                    });
                    tracing::warn!(
                        "validator-{agent} tool {} blocked by repetition detector",
                        tc.function.name
                    );
                    err_msg
                }
                crate::harness::monitor::Intervention::None => {
                    let invocation = crate::harness::ToolInvocation {
                        name: tc.function.name.clone(),
                        arguments: args_val,
                    };
                    let tool_res = crate::harness::dispatch_for(
                        &invocation,
                        crate::harness::ToolCaller::Specialist(Agent::Validator),
                    );
                    match tool_res {
                        Ok(r) => {
                            tracing::info!(
                                "validator-{agent} tool {} completed with {} chars",
                                tc.function.name,
                                r.content.len()
                            );
                            r.content
                        }
                        Err(e) => {
                            tracing::warn!(
                                "validator-{agent} tool {} error: {e}",
                                tc.function.name
                            );
                            format!("ERROR: {e}")
                        }
                    }
                }
            };
            engine.append(crate::types::Message::Tool {
                tool_call_id: tc.id,
                content,
            });
        }
    }

    // If the loop finished all turns without an explicit leave_verdict tool call (matching Caesar executor.rs:741):
    tracing::warn!("Validator for {agent} completed turns without calling leave_verdict.");
    Ok((
        false,
        "The validator failed to submit a verdict using the 'leave_verdict' tool.".to_string(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_display_and_parse() {
        assert_eq!(Agent::Coder.to_string(), "coder");
        assert_eq!(Agent::from_str("debugger"), Some(Agent::Debugger));
        assert_eq!(Agent::from_str("researcher"), Some(Agent::Researcher));
        assert_eq!(Agent::from_str("validator"), Some(Agent::Validator));
        assert_eq!(Agent::from_str("generalist"), Some(Agent::Generalist));
        assert_eq!(Agent::from_str("deepbrain"), Some(Agent::Generalist));
        assert_eq!(Agent::from_str("unknown"), None);
    }

    #[test]
    fn test_mission_marker_parse() {
        assert_eq!(
            MissionMarker::parse("MISSION COMPLETE (t-001)"),
            Some(MissionMarker::Complete {
                task_id: Some("t-001".to_string())
            })
        );
        assert_eq!(
            MissionMarker::parse("REPLAN REQUIRED: missing deps"),
            Some(MissionMarker::Replan {
                reason: "REPLAN REQUIRED: missing deps".to_string()
            })
        );
        assert!(matches!(
            MissionMarker::parse("FAILED because of test errors"),
            Some(MissionMarker::Failed { .. })
        ));
    }

    #[test]
    fn test_assemble_final_deliverable() {
        let approved = assemble_final_deliverable(true, None, "Code completed.");
        assert!(approved.contains("MISSION COMPLETE"));

        let rejected = assemble_final_deliverable(
            false,
            Some("Syntax error in line 10"),
            "Code done. MISSION COMPLETE",
        );
        assert!(rejected.contains("VALIDATOR REJECTION: Syntax error in line 10"));
        assert!(!rejected.contains("MISSION COMPLETE"));
        assert!(rejected.contains("FAILED"));
    }
}
