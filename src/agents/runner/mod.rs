//! Specialist live execution runner, turn loops, and deliverable assembly.

pub mod assembly;
pub mod execution;
pub mod formatting;

pub use assembly::*;
pub use execution::*;
pub use formatting::*;

use crate::agents::{Agent, IsolatedContext};

pub(crate) async fn run_specialist_llm(
    agent: Agent,
    ctx: &IsolatedContext,
    token: &tokio_util::sync::CancellationToken,
) -> String {
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

    if let Some(res) = try_run_specialist_live(agent, ctx, token).await {
        return res;
    }
    canned
}

pub(crate) async fn try_run_specialist_live(
    agent: Agent,
    ctx: &IsolatedContext,
    token: &tokio_util::sync::CancellationToken,
) -> Option<String> {
    if tokio::runtime::Handle::try_current().is_err() {
        return None;
    }
    // If running inside cargo test suite (integration tests binaries in target/.../deps/), bypass live network calls
    if std::env::current_exe()
        .map(|p| {
            let s = p.to_string_lossy();
            s.contains("/deps/") || s.contains(r"\deps\")
        })
        .unwrap_or(false)
        && std::env::var("MARMEL_LIVE_TEST").is_err()
    {
        return None;
    }
    let cfg = crate::config::get_active().or_else(|| crate::config::load(None).ok())?;
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
    let res = match run_specialist_live(&client, agent, ctx, &cfg, token).await {
        Ok(s) => s,
        Err(e) => format!("Specialist execution failed: {e}\n\nFAILED"),
    };
    Some(res)
}
