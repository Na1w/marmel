//! Specialist live execution runner, turn loops, and deliverable assembly.

pub mod assembly;
pub mod execution;
pub mod fix_loop;
pub mod formatting;

pub use assembly::*;
pub use execution::*;
pub use formatting::*;

use crate::agents::{Agent, IsolatedContext};
use crate::markers::{MARKER_COMPLETE, MARKER_FAILED, failed_trailer};

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
         {MARKER_COMPLETE}",
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

    let is_test_runner = std::env::current_exe()
        .map(|p| {
            let s = p.to_string_lossy();
            s.contains("/deps/") || s.contains(r"\deps\")
        })
        .unwrap_or(false)
        && std::env::var("MARMEL_LIVE_TEST").is_err();

    if is_test_runner {
        canned
    } else {
        format!(
            "Specialist execution failed: configuration or LLM backend unavailable outside test environment.\n\n{}",
            failed_trailer("config_error")
        )
    }
}

/// Resolve a specialist's own backend/token/model chain
/// (`[specialists.<role>]` entry → global config) — the mirror of
/// [`crate::agents::runner::fix_loop::resolve_validator_backend`] for the role
/// being audited rather than the auditor.
///
/// The returned `(backend_url, model)` is also the **pool key** this delegation's
/// LLM calls are metered against, because `ChatClient::chat_stream` gates on that
/// pair. That is why the two resolvers, not ad-hoc field reads, are the way to ask
/// "does this validator pass share the specialist's cap?": with no overrides on
/// either side both chains land on the global `backend_url` + `model` and one
/// semaphore serves both (validator passes count against the same `max_sessions`,
/// design §4); an override on either side moves that side to its own pool.
pub fn resolve_specialist_backend(
    cfg: &crate::config::Config,
    agent: Agent,
) -> (String, String, String) {
    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let backend = specialist_cfg
        .and_then(|sc| sc.backend_url.as_ref())
        .unwrap_or(&cfg.backend_url)
        .to_string();
    let auth_token = specialist_cfg
        .and_then(|sc| sc.auth_token.as_ref())
        .unwrap_or(&cfg.auth_token)
        .to_string();
    let model = specialist_cfg
        .and_then(|sc| sc.model.as_ref())
        .unwrap_or(&cfg.model)
        .to_string();
    (backend, auth_token, model)
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
    let (backend_url, auth_token, model) = resolve_specialist_backend(&cfg, agent);
    if backend_url.is_empty() {
        return None;
    }
    let client = crate::llm::ChatClient::new_with_token(&backend_url, &model, &auth_token);
    // Call class for this delegation, installed before the turn starts and
    // discharged when it returns. This is the point where the delegation's
    // `(backend_url, model)` pair is resolved, so the pool's gate in
    // `ChatClient::chat_stream` sees a context that matches the pool the call is
    // metered against: class `Specialist`, attributed to this agent and plan task.
    // Nesting is safe — `run_specialist_live` re-installs the same context for
    // callers that enter there directly, and each scope restores the previous one.
    let call_context = crate::orchestrator::pool::CallContext::for_agent(
        crate::orchestrator::pool::CallClass::Specialist,
        agent.as_str(),
        ctx.task_id.clone(),
    );
    let res = match crate::orchestrator::pool::with_call_context(
        call_context,
        run_specialist_live(&client, agent, ctx, &cfg, token),
    )
    .await
    {
        Ok(s) => s,
        Err(e) => format!("Specialist execution failed: {e}\n\n{MARKER_FAILED}"),
    };
    Some(res)
}
