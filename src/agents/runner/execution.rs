//! Specialist execution loop and tool turn state machine.

use super::assembly::{assemble_final_deliverable, update_revision};
use crate::agents::validation::{
    NO_EXPLICIT_APPROVAL_REASON, is_leave_verdict_tool, parse_verdict_args,
    run_automated_validation,
};
use crate::agents::{Agent, IsolatedContext};
use crate::markers::{MARKER_COMPLETE, MARKER_REPLAN, has_replan_marker, has_terminal_marker};
use crate::session::worker_journal::{
    self, WorkerSeed, mirror_append, mirror_collapse, mirror_replace_last,
};
use crate::tool_names::TOOL_LEAVE_VERDICT;

const MAX_CONSECUTIVE_THINKING_NUDGES: u32 = 5;
const MAX_CONSECUTIVE_MALFORMED_TOOL_CALLS: u32 = 5;
/// How often one worker may be refused by the verdict role check before its
/// deliverable is failed outright. Bounded so a model that insists on approving
/// itself cannot spin the turn loop forever.
const MAX_CONSECUTIVE_VERDICT_ROLE_REJECTIONS: u32 = 3;

/// Deliverable handed back when the user cancels a specialist mid-turn. The
/// whole body — the `Task …` sentence as well as the verdict trailer — comes
/// from [`crate::markers::aborted_deliverable`], so both the wording and the
/// marker spelling stay owned by `crate::markers`.
fn aborted_deliverable() -> String {
    crate::markers::aborted_deliverable("aborted by user instruction")
}

/// Notice injected after a successful `rebirth` checkpoint. It instructs the
/// specialist how to conclude, so the marker is spelled from
/// [`MARKER_COMPLETE`] rather than re-typed here.
fn rebirth_notice() -> String {
    format!(
        "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do not call rebirth consecutively without making progress. Proceed immediately using your required tools to perform the task and conclude with '{MARKER_COMPLETE}')."
    )
}

/// May this caller role record a validation verdict at all? **Only the validator
/// role**, and only when the specialist registry (`caller_allows_tool` →
/// `SpecialistEntry::allows`) also allows the verdict tool for that role. The
/// registry check alone is not sufficient — the generalist namespace is `*` — and
/// prompt/blueprint allowlists are advisory, so this check overrides wildcard
/// allowlists and runs *before* any dispatch, the way `create_plan` is
/// Manager-only even for a wildcard (`*`) specialist.
pub fn may_record_verdict(
    agent: Agent,
    registry: &crate::orchestrator::SpecialistRegistry,
) -> bool {
    agent == Agent::Validator
        && crate::orchestrator::caller_allows_tool(agent, TOOL_LEAVE_VERDICT, registry)
}

/// The caller identity of one specialist run, resolved **once** per run and
/// reused by every dispatch in the turn loop. What the model is *offered* and
/// what it is *allowed to execute* are judged for exactly the same caller and can
/// never drift apart.
pub fn specialist_caller(agent: Agent, ctx: &IsolatedContext) -> crate::harness::ToolCaller {
    match ctx.allowed_tools() {
        Some(allowed) => crate::harness::ToolCaller::SpecialistWithTools {
            agent,
            allowed_tools: allowed.to_vec(),
        },
        None => crate::harness::ToolCaller::Specialist(agent),
    }
}

/// The tool schemas one specialist run is **offered**: assembled by
/// [`super::fix_loop::assemble_tools_for_caller`] (its blueprint first filtered
/// through [`super::fix_loop::role_filtered_blueprint`], never the raw
/// [`super::fix_loop::assemble_tools`] plus wildcard registry namespaces) and then
/// narrowed by [`super::fix_loop::advertised_tools_for_caller`], which drops the
/// whole verdict class for any caller [`may_record_verdict`] refuses: no name
/// matched by [`super::fix_loop::is_verdict_recording_tool`] reaches a worker, while
/// [`Agent::Validator`] keeps its verdict tool under [`may_record_verdict`].
pub fn specialist_advertised_tools(
    caller: &crate::harness::ToolCaller,
    entry_allows: impl Fn(&str) -> bool,
    mcp_servers: &[String],
) -> Vec<crate::types::ToolDef> {
    let assembled = super::fix_loop::assemble_tools_for_caller(caller, entry_allows, mcp_servers);
    super::fix_loop::advertised_tools_for_caller(&assembled, caller)
}

/// Build the context engine of one specialist run — from
/// [`crate::manager::ContextEngineFactory::specialist_context`] — and charge it with
/// the exact tool schemas that run advertises, through
/// [`super::fix_loop::charge_engine_tool_schema`]: the crate's one implementation of
/// "price the advertised view". `advertised` **must** be the very list the run hands
/// to [`super::fix_loop::build_turn_request`] — a superset would trim transcript the
/// wire never paid for, a subset would under-price the request and let an
/// over-budget turn through. The Manager path charges the same way at construction.
pub fn build_specialist_context(
    cfg: &crate::config::Config,
    enhanced_system_prompt: String,
    brief: String,
    advertised: &[crate::types::ToolDef],
) -> crate::manager::ContextEngine {
    let mut engine = crate::manager::ContextEngineFactory::new(cfg.max_context_tokens)
        .specialist_context(enhanced_system_prompt, brief);
    super::fix_loop::charge_engine_tool_schema(&mut engine, advertised);
    engine
}

/// Re-charge `engine` with the advertised view it is **about to send** and return
/// the charged schema token count. A specialist's advertised list is not static: MCP
/// servers boot again and the advertised MCP view is policy-filtered, so the loop
/// re-declares the list immediately before every budget decision instead of
/// trusting the construction-time charge — the same pattern
/// `src/ui/session.rs::sync_manager_tool_schema` applies to the Manager engine, in
/// one call to [`super::fix_loop::charge_engine_tool_schema`].
pub fn sync_specialist_tool_schema(
    engine: &mut crate::manager::ContextEngine,
    advertised: &[crate::types::ToolDef],
) -> usize {
    super::fix_loop::charge_engine_tool_schema(engine, advertised)
}

pub async fn run_specialist_live(
    client: &crate::llm::ChatClient,
    agent: Agent,
    ctx: &IsolatedContext,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<String> {
    // Call class for every backend call this turn makes, attributed to this agent
    // and its plan task (design §1.5). The validator/fix-loop passes nested
    // inside this turn are **re-metered** as `CallClass::Validator` by the fix-loop
    // driver (`fix_loop::fix_loop_call_context`), so a queued auditor pass is
    // never blamed on the specialist it audits — but they still draw from the same
    // `Validator`-capped pool for their own resolved `backend + model` pair.
    // This is also the *direct* specialist entry point (public API, driven
    // directly by integration tests), so the class is installed here as well as at
    // the delegation entry in `super::try_run_specialist_live`.
    //
    // It is a task-local scope, not a guard: it stays in force across every await
    // of this turn on whichever tokio worker polls it, and it is discharged the
    // moment the turn returns, cancels or unwinds — nothing survives for the next
    // call on this task.
    let call_context = crate::orchestrator::pool::CallContext::for_agent(
        crate::orchestrator::pool::CallClass::Specialist,
        agent.as_str(),
        ctx.task_id.clone(),
    );
    crate::orchestrator::pool::with_call_context(
        call_context,
        crate::orchestrator::CURRENT_WORKER_TOKEN.scope(
            token.clone(),
            run_specialist_live_inner(client, agent, ctx, cfg, token),
        ),
    )
    .await
}

async fn run_specialist_live_inner(
    client: &crate::llm::ChatClient,
    agent: Agent,
    ctx: &IsolatedContext,
    cfg: &crate::config::Config,
    token: &tokio_util::sync::CancellationToken,
) -> anyhow::Result<String> {
    // Resolve the caller identity once, before anything is advertised.
    let caller = specialist_caller(agent, ctx);

    let tools_list = if let Some(ref bp) = ctx.blueprint {
        // The prose tool list is advertising too: a verdict-recording tool is
        // dropped for every caller the public verdict check refuses, through the
        // same helper the fix-loop blueprint path uses (no second filter).
        super::fix_loop::role_filtered_blueprint(&caller, &bp.allowed_tools).join("`, `")
    } else {
        "write_file`, `replace`, `read_file`, `run_command`, `grep_search`, `glob`, `rebirth"
            .to_string()
    };

    let env_block = crate::prompts::format_environment_block();
    let enhanced_system_prompt = format!(
        "{}\n\n{}\n- Tools available: `{}`.\n- You MUST save files and execute real work to complete the task.\n- Context Preservation: If context usage is high (>= 80%) or advised, call `rebirth` with a detailed summary capturing all pertinent state (including active file paths, current read offsets or line numbers reached in `read_file`, intermediate findings, and next actions) so work resumes seamlessly without starting over.",
        ctx.role_system_prompt, env_block, tools_list
    );

    let specialist_cfg = cfg.orchestration.specialists.get(agent.as_str());
    let specialist_model = specialist_cfg
        .and_then(|sc| sc.model.as_ref())
        .cloned()
        .unwrap_or_else(|| cfg.model.clone());
    // The reasoning-effort **dial** of this specialist's turns, resolved with the
    // same per-role → top-level precedence the thinking cap below uses
    // (`max_thinking_tokens` at the turn site is the ceiling; this is the dial,
    // and both can be set). `None` ⇒ the key is omitted from the wire body, and a
    // blank configured value **counts as unset** — it is normalized away here so
    // the top-level value still applies, and normalized again at the request
    // chokepoint (`fix_loop::build_turn_request`).
    // No other role's value is consulted here: validator passes get their own
    // chain (`resolve_validator_backend`) and the Manager gets the top-level
    // value through `StreamConfig`.
    let specialist_reasoning_effort = specialist_cfg
        .and_then(|sc| super::fix_loop::normalized_reasoning_effort(sc.reasoning_effort.as_deref()))
        .or_else(|| super::fix_loop::normalized_reasoning_effort(cfg.reasoning_effort.as_deref()));

    let clean_task_id = ctx
        .task_id
        .as_deref()
        .and_then(crate::task_id::normalize_task_id);

    let agent_tag = match &clean_task_id {
        Some(t) => format!("{agent}-{t}"),
        None => format!("{agent}"),
    };

    let registry = crate::orchestrator::SpecialistRegistry::canonical();
    let reg_entry = registry.resolve(agent).ok_or_else(|| {
        anyhow::anyhow!(
            "no specialist registry entry registered for agent {}; cannot assemble its tools",
            agent.as_str()
        )
    })?;
    // Shared tool-assembly helper (single owner): default tools filtered by the
    // blueprint allow-list / registry namespaces + MCP fan-out, then made role-aware
    // for advertising: the verdict class is never *offered* to a caller
    // [`may_record_verdict`] refuses — not through a prompt blueprint, not through a
    // wildcard registry namespace such as the Generalist's `"*"` — while
    // [`Agent::Validator`] keeps its verdict tool. The dispatcher-side check below
    // stays as the second line of defence.
    let mcp_servers = specialist_cfg
        .map(|sc| sc.mcp_servers.clone())
        .unwrap_or_default();
    let tools = specialist_advertised_tools(&caller, |name| reg_entry.allows(name), &mcp_servers);

    // The engine is built **through** `build_specialist_context`, on the very line
    // the advertised list exists, so it is charged with exactly that list before the
    // first `build_turn_request` and before any budget decision. An uncharged engine
    // reports only the message-only `request_token_count()`, which `context.rs`
    // documents as a **lower bound**, while `should_compact()` /
    // `should_advise_rebirth()` price a request thousands of tokens larger on the
    // wire. The list is moved here, not re-derived, so charge and wire stay identical.
    let mut engine =
        build_specialist_context(cfg, enhanced_system_prompt, ctx.brief.clone(), &tools);

    if !ctx.snippets.is_empty() {
        let snippet_text = format!("Snippets:\n{}", ctx.snippets.join("\n---\n"));
        engine.append(crate::types::Message::User {
            content: snippet_text,
        });
    }

    let mut final_content = String::new();
    let mut nudge_count = 0u32;
    let mut consecutive_thinking_nudges = 0u32;
    let mut consecutive_malformed_tool_calls = 0u32;
    let mut verdict_role_rejections = 0u32;

    let _active_guard = crate::orchestrator::register_active_worker_with_token(
        clean_task_id.clone(),
        agent.as_str().to_string(),
        ctx.brief.clone(),
        Some(token.clone()),
    );

    // The engine above is a **local**, so nothing outside this function can read the
    // worker's history once the run ends. The journal is an append-side *mirror* of
    // it, opened under the live worker key and seeded with what the engine already
    // holds (system prompt, brief, snippets block) plus the delegation seed. It is a
    // shadow only: the engine stays authoritative for execution, the mirror never
    // touches disk, and every mirrored step is a clone on this thread.
    let journal = worker_journal::open(
        &_active_guard.0,
        crate::orchestrator::workers::worker_state_snapshot(&_active_guard.0)
            .map(|state| state.worker_id_seq_value)
            .unwrap_or(0),
        cfg.session_worker_mirror_max_messages,
    );
    journal.record_seed(&engine, &WorkerSeed::from_context(ctx));
    // Frozen when the run ends, whichever of the many exit paths it takes.
    let _journal_close = worker_journal::JournalCloseGuard::new(journal.clone());

    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor = crate::harness::monitor::HarnessMonitor::new_with_config(
        std::sync::Arc::new(crate::harness::HarnessStats::new()),
        mon_cfg,
    );
    let mut tools_executed_count = 0usize;
    let mut _turn = 0usize;

    let auto_validate_enabled = specialist_cfg
        .and_then(|sc| sc.enable_validator)
        .unwrap_or(true);
    let max_val_iterations = specialist_cfg
        .and_then(|sc| sc.max_validator_iterations)
        .unwrap_or(5);

    let ws_root = crate::harness::get_workspace_root();
    let prompts_dir = ws_root
        .join(crate::manager::phase::MARMEL_DIR)
        .join("prompts");
    let has_prompts_dir = prompts_dir.is_dir();

    let validation_configured =
        auto_validate_enabled && agent != Agent::Validator && agent != Agent::Planner;

    // Fail closed: an absent, unreadable or blank verdict file
    // (`{tid}-validation.md` under the prompts directory) is an unconditional
    // validation failure for every task that requires validation: the deliverable is
    // reported as not validated and its plan line stays unchecked. No plan lookup, no
    // exception for untracked task ids; only `max_validator_iterations = 0` /
    // `enable_validator = false` opt out. The id is validated as a single path segment
    // before joining, so `../../etc/x` or `a/b` cannot reach a verdict file outside
    // the prompts directory; a rejected id is a verdict gap, never sanitized/clamped.
    let verdict_gap: Option<String> = if has_prompts_dir
        && validation_configured
        && max_val_iterations > 0
    {
        match clean_task_id.as_deref() {
            None => Some(format!(
                "the run carries no task id, so no `-validation.md` verdict file could be located under {}",
                prompts_dir.display()
            )),
            Some(tid) => match crate::task_id::validate_task_id(tid) {
                Err(err) => Some(format!(
                    "the task id {tid:?} was rejected as a single path segment ({err}), so no `-validation.md` verdict file could be located under {}",
                    prompts_dir.display()
                )),
                Ok(id) => {
                    let verdict_path = prompts_dir.join(format!("{id}-validation.md"));
                    match std::fs::read_to_string(&verdict_path) {
                        Err(err) if err.kind() == std::io::ErrorKind::NotFound => Some(format!(
                            "{} does not exist, so no validation verdict is recorded for this task",
                            verdict_path.display()
                        )),
                        Err(err) => Some(format!(
                            "{} could not be read ({err}), so its validation verdict is unreadable",
                            verdict_path.display()
                        )),
                        Ok(text) if text.trim().is_empty() => Some(format!(
                            "{} is empty, so no validation verdict is recorded for this task",
                            verdict_path.display()
                        )),
                        Ok(_) => None,
                    }
                }
            },
        }
    } else {
        None
    };

    // Every task that requires validation is checked, and a verdict gap is carried
    // into that check as a hard failure regardless of whether the execution plan
    // happens to track the task id.
    let requires_validation = validation_configured && max_val_iterations > 0;
    let missing_verdict_reason: Option<String> = verdict_gap;

    // `max_val_iterations == 0` is already folded into `requires_validation`
    // above, so the opt-out needs no second spelling here.
    let mut validation_passed = !requires_validation;
    let mut validator_critique: Option<String> = None;
    let mut val_iter = 0usize;

    loop {
        _turn += 1;
        if token.is_cancelled() || crate::orchestrator::is_current_or_global_cancelled() {
            tracing::warn!("{agent_tag}: aborted by cancellation signal");
            crate::orchestrator::set_active_worker_status(&_active_guard.0, "Aborted");
            return Ok(aborted_deliverable());
        }
        let mut rep_detector = crate::harness::monitor::RepetitionDetector::new(
            mon_cfg.repetition_threshold,
            mon_cfg.min_pattern_len,
        );
        crate::orchestrator::update_active_worker_progress(
            &_active_guard.0,
            _turn,
            val_iter,
            validator_critique.clone(),
        );
        crate::orchestrator::update_active_worker_context(&_active_guard.0, engine.token_count());

        // Turn-start notice drain — rendered by the crate's single notice renderer.
        // The injector mutates the engine, so it mirrors through the task-local
        // handle.
        worker_journal::with_journal(&journal, || {
            super::fix_loop::inject_worker_notices_at_turn_start(&mut engine, &_active_guard.0);
        });

        crate::orchestrator::emit_status(format!(
            "{agent_tag}: thinking / calling model ({specialist_model})..."
        ));
        // Shared turn-request builder + shared streaming plumbing (one owner in `fix_loop`).
        let req = super::fix_loop::build_turn_request(
            &specialist_model,
            &engine,
            &tools,
            cfg,
            cfg.temperature,
            specialist_reasoning_effort.as_deref(),
        );
        let max_tokens = mon_cfg.max_stream_tokens.max(256);
        let max_thinking_tokens = specialist_cfg
            .and_then(|s| s.max_thinking_tokens)
            .unwrap_or(cfg.max_thinking_tokens)
            .max(256);
        let stream_out = super::fix_loop::stream_single_turn(
            client,
            &req,
            &agent_tag,
            agent.as_str(),
            &specialist_model,
            clean_task_id.clone(),
            token,
            max_tokens,
            max_thinking_tokens,
            &mut rep_detector,
        )
        .await;

        let out = match stream_out {
            Ok(Some(o)) => o,
            Ok(None) => {
                // Strict abort semantics (fix_loop module docs): cancellation is
                // surfaced as a FAILED deliverable, never silently swallowed.
                tracing::warn!("{agent_tag}: aborted during LLM call");
                crate::orchestrator::set_active_worker_status(&_active_guard.0, "Aborted");
                return Ok(aborted_deliverable());
            }
            Err(e) => return Err(e),
        };

        let reply = out.reply;
        let budget_exceeded = out.budget_exceeded;
        let thinking_budget_exceeded = out.thinking_budget_exceeded;
        let rep_triggered = out.rep_triggered;
        if budget_exceeded {
            tracing::warn!(
                "{agent_tag}: maximum single-turn output budget of {max_tokens} tokens exceeded — cutting stream"
            );
            crate::orchestrator::emit_status(format!(
                "{agent_tag}: single-turn output budget ({max_tokens} tokens) reached"
            ));
        }
        if thinking_budget_exceeded {
            tracing::warn!(
                "{agent_tag}: maximum single-turn reasoning budget of {max_thinking_tokens} tokens exceeded — cutting stream"
            );
            crate::orchestrator::emit_status(format!(
                "{agent_tag}: single-turn reasoning budget ({max_thinking_tokens} tokens) reached"
            ));
        }
        update_revision(&mut final_content, &reply.content);

        let mut tool_calls = reply.tool_calls.clone();
        if tool_calls.is_empty() && cfg.enable_xml_rescue {
            let rescued = monitor.rescue_xml(&reply.content);
            if !rescued.is_empty() {
                tool_calls = rescued;
            }
        }

        let assistant_content = if reply.content.is_empty() {
            if tool_calls.is_empty() && !reply.reasoning.is_empty() {
                Some("[Thinking completed without content or tool calls]".to_string())
            } else {
                Some(String::new())
            }
        } else {
            Some(reply.content.clone())
        };
        let assistant_msg = crate::types::Message::Assistant {
            content: assistant_content,
            reasoning_content: if reply.reasoning.is_empty() {
                None
            } else {
                Some(reply.reasoning.clone())
            },
            tool_calls: tool_calls.clone(),
        };
        mirror_append(&mut engine, Some(&journal), assistant_msg);

        let is_repeating = rep_triggered || monitor.feed_text(&reply.content);

        if !thinking_budget_exceeded || !tool_calls.is_empty() {
            consecutive_thinking_nudges = 0;
        }

        if tool_calls.is_empty() {
            if thinking_budget_exceeded {
                consecutive_thinking_nudges += 1;
                if consecutive_thinking_nudges >= MAX_CONSECUTIVE_THINKING_NUDGES {
                    tracing::warn!(
                        "{agent_tag}: thinking budget exceeded {MAX_CONSECUTIVE_THINKING_NUDGES} times consecutively — returning {MARKER_REPLAN}"
                    );
                    crate::orchestrator::emit_status(format!(
                        "{agent_tag}: reasoning budget exceeded {MAX_CONSECUTIVE_THINKING_NUDGES} times consecutively — task too complex, requesting replan"
                    ));
                    crate::orchestrator::set_active_worker_status(
                        &_active_guard.0,
                        "Replan Required (task too complex)",
                    );
                    let task_ref = ctx.task_id.as_deref().unwrap_or("task");
                    let replan_msg = format!(
                        "{MARKER_REPLAN} ({task_ref}): task too complex — exceeded single-turn reasoning budget of {max_thinking_tokens} tokens {MAX_CONSECUTIVE_THINKING_NUDGES} times consecutively without completing work."
                    );
                    return Ok(replan_msg);
                }
                tracing::warn!(
                    "{agent_tag}: thinking budget exceeded — injecting reasoning cutoff nudge ({consecutive_thinking_nudges}/{MAX_CONSECUTIVE_THINKING_NUDGES})"
                );
                crate::orchestrator::emit_status(format!(
                    "{agent_tag}: reasoning budget ({max_thinking_tokens} tokens) reached — nudging out of thinking"
                ));
                mirror_replace_last(
                    &mut engine,
                    Some(&journal),
                    crate::types::Message::Assistant {
                        content: if reply.content.trim().is_empty() {
                            Some(format!(
                                "[Reasoning budget reached: exceeded {max_thinking_tokens} token limit]"
                            ))
                        } else {
                            Some(reply.content.clone())
                        },
                        reasoning_content: if reply.reasoning.is_empty() {
                            None
                        } else {
                            Some(reply.reasoning.clone())
                        },
                        tool_calls: Vec::new(),
                    },
                );
                mirror_append(
                    &mut engine,
                    Some(&journal),
                    crate::types::Message::User {
                        content: format!(
                            "SYSTEM NOTICE: Maximum reasoning budget of {max_thinking_tokens} tokens reached for this turn. Stop internal thinking immediately. Proceed directly to output your deliverables or execute required tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.)."
                        ),
                    },
                );
                continue;
            }

            if budget_exceeded && nudge_count < 2 {
                nudge_count += 1;
                tracing::warn!(
                    "{agent_tag}: output budget exceeded — injecting corrective nudge ({nudge_count}/2)"
                );
                mirror_replace_last(
                    &mut engine,
                    Some(&journal),
                    crate::types::Message::Assistant {
                        content: Some(format!(
                            "[Generation truncated: exceeded {max_tokens} token single-turn limit]"
                        )),
                        reasoning_content: None,
                        tool_calls: Vec::new(),
                    },
                );
                mirror_append(
                    &mut engine,
                    Some(&journal),
                    crate::types::Message::User {
                        content: format!(
                            "SYSTEM NOTICE: Your response exceeded the single-turn output budget limit ({max_tokens} tokens) and was truncated. Please be concise, call your required tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.) to perform the work, or conclude with '{MARKER_COMPLETE}'."
                        ),
                    },
                );
                continue;
            }

            if is_repeating {
                if nudge_count < 2 {
                    nudge_count += 1;
                    tracing::warn!(
                        "{agent_tag}: repetitive generation loop detected in specialist output — injecting corrective nudge ({nudge_count}/2)"
                    );
                    mirror_replace_last(
                        &mut engine,
                        Some(&journal),
                        crate::types::Message::Assistant {
                            content: Some(
                                "[Generation interrupted due to repetitive loop]".to_string(),
                            ),
                            reasoning_content: None,
                            tool_calls: Vec::new(),
                        },
                    );
                    rep_detector = crate::harness::monitor::RepetitionDetector::new(
                        mon_cfg.repetition_threshold,
                        mon_cfg.min_pattern_len,
                    );
                    mirror_append(
                        &mut engine,
                        Some(&journal),
                        crate::types::Message::User {
                            content: format!(
                                "SYSTEM NOTICE: Repetitive generation loop detected in your responses. Terminate conversational debate immediately and invoke your required tools (such as `read_file`, `write_file`, `run_command`, etc.) to perform the required work, or conclude with '{MARKER_COMPLETE}'."
                            ),
                        },
                    );
                    continue;
                } else {
                    tracing::warn!(
                        "{agent_tag}: repetitive generation loop persisted across turns — terminating specialist loop"
                    );
                    break;
                }
            }

            let is_terminal = has_terminal_marker(&reply.content);
            if !is_terminal {
                if nudge_count < 2 {
                    nudge_count += 1;
                    let nudge_msg = if reply.content.trim().is_empty()
                        && !reply.reasoning.is_empty()
                    {
                        format!(
                            "SYSTEM NOTICE: Your thoughts completed but you produced 0 output text and 0 tool calls. Do not remain silent in thoughts. You MUST execute your required tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.) to write files to disk and perform the task, or conclude with '{MARKER_COMPLETE}'."
                        )
                    } else {
                        format!(
                            "SYSTEM NOTICE: You did not call any tools or output {MARKER_COMPLETE}. Do not output conversational prose. Immediately use your tools (such as `read_file`, `write_file`, `replace`, `run_command`, etc.) to perform the required work, create/update any requested files in the workspace, and conclude with '{MARKER_COMPLETE}'."
                        )
                    };
                    mirror_append(
                        &mut engine,
                        Some(&journal),
                        crate::types::Message::User { content: nudge_msg },
                    );
                    continue;
                } else {
                    tracing::warn!(
                        "{agent_tag}: specialist produced no tool calls after {nudge_count} nudges — terminating"
                    );
                    break;
                }
            }

            let has_verdict = has_terminal_marker(&reply.content);

            if tools_executed_count == 0 && !has_verdict {
                tracing::warn!(
                    "{agent_tag}: specialist produced no tool executions or terminal marker — failing deliverable without validation"
                );
                crate::orchestrator::set_active_worker_status(
                    &_active_guard.0,
                    "Failed (no tools executed)",
                );
                return Ok(assemble_final_deliverable(
                    false,
                    Some("Specialist generated conversational text without executing any tools."),
                    &final_content,
                    ctx.task_id.as_deref(),
                ));
            }

            if final_content.trim().is_empty() {
                if !reply.reasoning.trim().is_empty() {
                    final_content = reply.reasoning.clone();
                } else if tools_executed_count > 0 {
                    final_content = format!(
                        "Specialist executed {tools_executed_count} tool operations to complete the task."
                    );
                }
            }

            if requires_validation
                && !final_content.is_empty()
                && (tools_executed_count > 0 || has_verdict)
                && !has_replan_marker(&reply.content)
            {
                // Hard failure: without a recorded per-task verdict file the
                // validator has no brief, so the deliverable must be reported as
                // *not validated* — never as silently validated.
                if let Some(gap) = missing_verdict_reason.as_deref() {
                    let critique = format!(
                        "Validation was not performed — {gap}. The deliverable is reported as not validated."
                    );
                    tracing::error!("{agent_tag}: {critique}");
                    crate::orchestrator::emit_status(format!(
                        "[Validator] NO VERDICT FILE for {agent_tag}: validation could not run — deliverable reported as not validated"
                    ));
                    validation_passed = false;
                    validator_critique = Some(critique.clone());
                    crate::orchestrator::update_active_worker_progress(
                        &_active_guard.0,
                        _turn,
                        val_iter,
                        Some(critique),
                    );
                    crate::orchestrator::set_active_worker_status(
                        &_active_guard.0,
                        "Failed (no validation verdict file)",
                    );
                    break;
                }
                if val_iter < max_val_iterations {
                    val_iter += 1;
                    if token.is_cancelled() || crate::orchestrator::is_current_or_global_cancelled()
                    {
                        tracing::warn!("{agent_tag}: aborted before validation pass");
                        crate::orchestrator::set_active_worker_status(&_active_guard.0, "Aborted");
                        return Ok(aborted_deliverable());
                    }
                    crate::orchestrator::emit_status(format!(
                        "validator-{agent_tag}: testing deliverable (pass {val_iter}/{max_val_iterations})..."
                    ));
                    match run_automated_validation(
                        client,
                        agent,
                        clean_task_id.as_deref(),
                        &ctx.brief,
                        &final_content,
                        cfg,
                        token,
                    )
                    .await
                    {
                        Ok((approved, critique)) => {
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
                                validator_critique = Some(feedback.clone());
                                crate::orchestrator::update_active_worker_progress(
                                    &_active_guard.0,
                                    _turn,
                                    val_iter,
                                    Some(feedback),
                                );
                                crate::orchestrator::set_active_worker_status(
                                    &_active_guard.0,
                                    "Approved",
                                );
                                break;
                            } else {
                                let feedback = if critique.trim().is_empty() {
                                    "Deliverable failed verification checks.".to_string()
                                } else {
                                    critique.clone()
                                };
                                validator_critique = Some(feedback.clone());
                                crate::orchestrator::emit_status(format!(
                                    "[Validator] REJECTED deliverable for {agent_tag} (pass {val_iter}/{max_val_iterations}):\n{feedback}"
                                ));
                                tracing::warn!(
                                    "Automated validator REJECTED specialist deliverable for {}: {}",
                                    agent_tag,
                                    feedback
                                );
                                crate::orchestrator::update_active_worker_progress(
                                    &_active_guard.0,
                                    _turn,
                                    val_iter,
                                    Some(feedback.clone()),
                                );
                                crate::orchestrator::set_active_worker_status(
                                    &_active_guard.0,
                                    &format!(
                                        "Revising (rejected pass {val_iter}/{max_val_iterations})"
                                    ),
                                );
                                let feedback_msg = format!(
                                    "Validation feedback: The validator tested your changes and found issues:\n{}\n\n\
                                     Please address all validator critique points, verify your work with available tools, and conclude with '{MARKER_COMPLETE}'.",
                                    feedback
                                );
                                mirror_append(
                                    &mut engine,
                                    Some(&journal),
                                    crate::types::Message::User {
                                        content: feedback_msg,
                                    },
                                );
                                nudge_count = 0;
                                continue;
                            }
                        }
                        Err(e) => {
                            tracing::warn!("Automated validator encountered error: {e}");
                            if token.is_cancelled()
                                || crate::orchestrator::is_current_or_global_cancelled()
                            {
                                crate::orchestrator::set_active_worker_status(
                                    &_active_guard.0,
                                    "Aborted",
                                );
                                return Ok(aborted_deliverable());
                            }
                            break;
                        }
                    }
                } else {
                    tracing::warn!(
                        "{agent_tag}: maximum validator iterations ({max_val_iterations}) reached without approval"
                    );
                    validation_passed = false;
                    crate::orchestrator::set_active_worker_status(
                        &_active_guard.0,
                        "Failed (max validator iterations exceeded)",
                    );
                    break;
                }
            } else {
                break;
            }
        }

        let mut turn_had_malformed_tool_call = false;
        let mut turn_had_successful_tool_call = false;
        let mut leave_verdict_called = false;
        for tc in tool_calls {
            if tc.is_malformed() {
                turn_had_malformed_tool_call = true;
            }
            if token.is_cancelled() || crate::orchestrator::is_current_or_global_cancelled() {
                tracing::warn!(
                    "{agent_tag}: aborted before executing tool {}",
                    tc.function.name
                );
                crate::orchestrator::set_active_worker_status(&_active_guard.0, "Aborted");
                return Ok(aborted_deliverable());
            }
            let args_val = serde_json::from_str::<serde_json::Value>(&tc.function.arguments)
                .unwrap_or_else(|_| serde_json::Value::String(tc.function.arguments.clone()));

            if is_leave_verdict_tool(&tc.function.name) {
                // Only the validator role may record a verdict — a worker calling
                // the verdict tool is refusing to audit itself (see
                // [`may_record_verdict`] for why the registry allowlist and the
                // prompt blueprint are each insufficient on their own). The refusal
                // is an explicit tool error visible to the worker (never a panic,
                // never a silent ignore) and the call is never dispatched: no verdict
                // state and no verdict file is touched, and the deliverable still has
                // to pass the automated validator. Repeated attempts fail the run.
                if !may_record_verdict(agent, &registry) {
                    verdict_role_rejections += 1;
                    let forbidden = crate::harness::ToolError::Forbidden {
                        tool: TOOL_LEAVE_VERDICT.to_string(),
                        caller: agent.as_str().to_string(),
                    };
                    if verdict_role_rejections >= MAX_CONSECUTIVE_VERDICT_ROLE_REJECTIONS {
                        let reason = format!(
                            "{} attempted to record its own validation verdict {verdict_role_rejections} times; only the {} role may record verdicts.",
                            agent.as_str(),
                            Agent::Validator.as_str()
                        );
                        tracing::warn!("{agent_tag}: verdict role violation — {reason}");
                        crate::orchestrator::set_active_worker_status(
                            &_active_guard.0,
                            "Failed (verdict role violation)",
                        );
                        return Ok(assemble_final_deliverable(
                            false,
                            Some(&reason),
                            &final_content,
                            ctx.task_id.as_deref(),
                        ));
                    }
                    let rejection = format!(
                        "ERROR: VERDICT REJECTED — {forbidden}. Only the {} role may record a validation verdict: {} cannot approve or reject its own deliverable. No verdict was recorded and the verdict file was not modified — the deliverable must still pass the automated validator. Conclude with '{MARKER_COMPLETE}' instead.",
                        Agent::Validator.as_str(),
                        agent.as_str(),
                    );
                    tracing::warn!(
                        "{agent_tag}: verdict call rejected by role gate (caller={} may not record verdicts)",
                        agent.as_str()
                    );
                    crate::orchestrator::emit_status(format!(
                        "[{agent_tag}] VERDICT REJECTED: only {} may record validation verdicts",
                        Agent::Validator.as_str()
                    ));
                    worker_journal::with_journal(&journal, || {
                        super::fix_loop::append_tool_result(
                            &mut engine,
                            &tc,
                            rejection,
                            false,
                            &rebirth_notice(),
                        );
                    });
                    crate::orchestrator::update_active_worker_context(
                        &_active_guard.0,
                        engine.token_count(),
                    );
                    continue;
                }

                // Validator role: record the verdict. A payload with no explicit
                // verdict resolves to NOT approved (fail-closed), never to an
                // implicit `true`.
                let (approved, critique) = parse_verdict_args(&args_val)
                    .unwrap_or((false, NO_EXPLICIT_APPROVAL_REASON.to_string()));

                validation_passed = approved;
                validator_critique = Some(critique.clone());

                let verdict_str = if approved { "APPROVED" } else { "REJECTED" };
                crate::orchestrator::emit_status(format!(
                    "[{agent_tag}] {verdict_str} via {}:\n{critique}",
                    TOOL_LEAVE_VERDICT
                ));
                if approved {
                    crate::orchestrator::set_active_worker_status(&_active_guard.0, "Approved");
                } else {
                    crate::orchestrator::set_active_worker_status(&_active_guard.0, "Rejected");
                }
                crate::debug_log::log_validation_verdict(agent.as_str(), approved, &critique);

                let verdict_summary = if approved {
                    format!("Verdict: APPROVED\n\nComments:\n{critique}")
                } else {
                    format!("Verdict: REJECTED\n\nCritique:\n{critique}")
                };
                if final_content.trim().is_empty() {
                    final_content = verdict_summary;
                } else if !final_content.contains(&critique) {
                    final_content.push_str("\n\n");
                    final_content.push_str(&verdict_summary);
                }

                let invocation = crate::harness::ToolInvocation {
                    name: tc.function.name.clone(),
                    arguments: args_val,
                };
                // Same caller identity as the advertised tool list.
                let _ = crate::harness::dispatch_for_async_with_engine(
                    &invocation,
                    caller.clone(),
                    Some(&mut engine),
                )
                .await;

                tools_executed_count += 1;
                leave_verdict_called = true;
                break;
            }

            // Shared tool-dispatch core (one owner in `fix_loop`): monitor
            // intervention check → cancellation check → dispatch. `None` means
            // aborted. The caller is the very identity the advertised tool list was
            // built from, so no tool is offered that dispatch would not judge alike.
            let dispatched = super::fix_loop::dispatch_tool_call(
                &mut monitor,
                &tc,
                caller.clone(),
                &mut engine,
                token,
                &agent_tag,
                true,
                "ERROR: ",
                "Tool repetition detected for '{tool}'. Do not repeat identical calls — proceed with your task or save deliverables with write_file.",
            )
            .await;
            let (content, execution_succeeded) = match dispatched {
                Some(d) => {
                    // A tool counts as executed only when dispatch returned Ok — not
                    // on errors and not on repetition blocks.
                    if d.1 {
                        tools_executed_count += 1;
                        turn_had_successful_tool_call = true;
                        nudge_count = 0;
                    }
                    d
                }
                None => {
                    crate::orchestrator::set_active_worker_status(&_active_guard.0, "Aborted");
                    return Ok(aborted_deliverable());
                }
            };
            worker_journal::with_journal(&journal, || {
                super::fix_loop::append_tool_result(
                    &mut engine,
                    &tc,
                    content,
                    execution_succeeded,
                    &rebirth_notice(),
                );
            });
            crate::orchestrator::update_active_worker_context(
                &_active_guard.0,
                engine.token_count(),
            );
        }

        // Mid-turn notice drain: a steering notice posted while this turn was in
        // flight reaches the worker inside this SAME turn. The drain runs right after
        // the tool round, so the injected notice is part of the transcript before
        // compaction and before every exit path below (replan return, the
        // `leave_verdict` break). Routing is exact-identity, so a notice addressed to
        // another worker is never injected here.
        worker_journal::with_journal(&journal, || {
            super::fix_loop::inject_worker_notices_mid_turn(&mut engine, &_active_guard.0);
        });

        if turn_had_malformed_tool_call {
            consecutive_malformed_tool_calls += 1;
            if consecutive_malformed_tool_calls >= MAX_CONSECUTIVE_MALFORMED_TOOL_CALLS {
                tracing::warn!(
                    "{agent_tag}: model produced malformed/truncated tool calls {MAX_CONSECUTIVE_MALFORMED_TOOL_CALLS} times consecutively — returning {MARKER_REPLAN}"
                );
                crate::orchestrator::emit_status(format!(
                    "{agent_tag}: malformed tool calls ({MAX_CONSECUTIVE_MALFORMED_TOOL_CALLS} times consecutively) — requesting replan"
                ));
                crate::orchestrator::set_active_worker_status(
                    &_active_guard.0,
                    "Replan Required (repeated malformed tool calls)",
                );
                let task_ref = ctx.task_id.as_deref().unwrap_or("task");
                let replan_msg = format!(
                    "{MARKER_REPLAN} ({task_ref}): model repeatedly produced truncated or invalid tool calls {MAX_CONSECUTIVE_MALFORMED_TOOL_CALLS} times consecutively without generating valid arguments."
                );
                return Ok(replan_msg);
            }
        } else if turn_had_successful_tool_call || leave_verdict_called {
            consecutive_malformed_tool_calls = 0;
        }
        // The budget decision is priced against the **advertised view this loop is
        // about to send**: the list is re-declared immediately before the decision,
        // because a construction-time charge goes stale (MCP servers boot again and
        // the advertised MCP view is policy-filtered). `tools` is the same `Vec`
        // this loop hands to `super::fix_loop::build_turn_request`, so charge and
        // wire cannot drift. The `CompactionOutcome` is never discarded:
        // it is the only evidence of whether the 70% target was reached, and
        // `TargetUnreachable` means the run keeps working over budget.
        sync_specialist_tool_schema(&mut engine, &tools);
        if engine.should_compact() {
            let outcome = engine.compact();
            // The mirror follows the engine's collapse; it never re-expands the
            // history the worker itself discarded.
            mirror_collapse(&engine, Some(&journal));
            tracing::info!("{agent_tag}: automatic context compaction: {outcome:?}");
            if let Some(notice) = super::fix_loop::fix_loop_compaction_notice(&agent_tag, &outcome)
            {
                tracing::warn!("{notice}");
                crate::orchestrator::emit_status(notice);
            }
        } else if engine.should_advise_rebirth() {
            engine.inject_rebirth_advisory();
            mirror_collapse(&engine, Some(&journal));
        }
        if leave_verdict_called {
            tracing::info!(
                "{agent_tag}: leave_verdict concluded specialist inspection loop (approved={validation_passed})"
            );
            break;
        }
    }

    let has_verdict = has_terminal_marker(&final_content);
    if tools_executed_count == 0 && !has_verdict {
        tracing::warn!(
            "{agent_tag}: specialist produced no tool executions or terminal marker — failing deliverable without validation"
        );
        crate::orchestrator::set_active_worker_status(
            &_active_guard.0,
            "Failed (no tools executed)",
        );
        return Ok(assemble_final_deliverable(
            false,
            Some("Specialist generated conversational text without executing any tools."),
            &final_content,
            ctx.task_id.as_deref(),
        ));
    }

    if validation_passed {
        crate::orchestrator::set_active_worker_status(&_active_guard.0, "Approved");
    } else if validator_critique.is_some() {
        crate::orchestrator::set_active_worker_status(&_active_guard.0, "Rejected");
    } else {
        crate::orchestrator::set_active_worker_status(&_active_guard.0, "Completed");
    }

    if !final_content.is_empty() {
        let assembled = assemble_final_deliverable(
            validation_passed,
            validator_critique.as_deref(),
            &final_content,
            ctx.task_id.as_deref(),
        );
        Ok(assembled)
    } else if tools_executed_count > 0 {
        let synth = format!(
            "Specialist executed {tools_executed_count} tool operations to complete the task."
        );
        let assembled = assemble_final_deliverable(
            validation_passed,
            validator_critique.as_deref(),
            &synth,
            ctx.task_id.as_deref(),
        );
        Ok(assembled)
    } else {
        Ok(assemble_final_deliverable(
            false,
            Some("Specialist produced no output deliverable or tool executions"),
            &final_content,
            ctx.task_id.as_deref(),
        ))
    }
}
