//! Interactive session runner driving multi-turn manager and specialist execution.

use super::bridge::{
    RendererSink, SteerArbEvent, drain_steer_arbitration_events_with_transcript,
    spawn_steer_arbitration,
};
use super::helpers::*;
use super::{Event, Renderer, UiRecord, UiTranscript};
use crate::config::Config;
use crate::llm::{ChatClient, StreamConfig, chat_client_turn};
use crate::manager::context::{
    ABORTED_TOOL_RESULT, CompactionOutcome, ContextEngine, count_text_tokens, manager_wire_tools,
};
use crate::manager::r#loop::{DeadlineKind, FailureBudget, FailureVerdict, TurnWatchdog};
use crate::orchestrator::OrchestratorManager;
use crate::types::Message;
use anyhow::Result;
use std::sync::Arc;
use std::time::Duration;

/// Execution bounds applied to the live session loop (recon H5: "no failure
/// budget, no wall-clock bound").
///
/// Production always runs on [`SessionBounds::default()`]. The bounds are
/// deliberately **config-independent**: they are a runaway safety net for the
/// interactive loop, not a user-facing knob, and putting them in `marmel.toml`
/// would let a bad config silently remove the only bound that exists.
///
/// The explicit variant exists because a 600 s watchdog and a 3 h hard cap
/// cannot be exercised hermetically at their production values;
/// `tests/test_ui_session.rs` drives the *real* loop with small ones.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SessionBounds {
    /// Wall-clock watchdog: how long one turn may go with **no observable
    /// progress** before it is torn down (REQ-LOOP-002, wall-clock half).
    pub turn_idle: Duration,
    /// Absolute wall-clock bound for a single turn, progress or not.
    pub turn_hard_cap: Duration,
    /// How many failures of the same task/tool call are tolerated inside one
    /// session before the loop escalates and then refuses to retry.
    pub failure_threshold: u32,
}

impl Default for SessionBounds {
    fn default() -> Self {
        Self {
            turn_idle: Duration::from_secs(crate::manager::r#loop::TURN_WATCHDOG_SECS),
            turn_hard_cap: Duration::from_secs(crate::manager::r#loop::TURN_HARD_CAP_SECS),
            failure_threshold: crate::manager::r#loop::TASK_FAILURE_ESCALATION_THRESHOLD,
        }
    }
}

// ── Why a session execution bound ended a turn (recon M2 / residual defect 2) ──
//
// The session loop has several ways to stop a turn: the wall-clock halves of
// [`SessionBounds`], and the per-request **turn budget**
// ([`crate::manager::r#loop::MAX_TURNS`]). Until t-074 only the wall-clock exits
// said anything: `turn_count > MAX_TURNS` broke the loop **silently**, so an
// operator watched a session stop responding after the budget was spent and had
// nothing distinguishing that from a clean end-of-turn finish (recon finding M2,
// `docs/recon_bugs_manager.md`; residual defect 2 of the manager-cluster gate
// t-067). The vocabulary below is the single stop-reason type the loop carries:
// every bound exit produces one, it is surfaced through the session's existing
// status channel (renderer `Message`/`Status` event + a `Status` record in the
// UI transcript + `tracing::warn!`), and it exposes a stable machine-readable
// [`BoundStopReason::code`] so a caller, a log field, or the journaled line can
// tell "turn budget reached" apart from a clean finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundStopReason {
    /// A wall-clock half of [`SessionBounds`] tore the turn down. `text` is the
    /// user-visible wording owned by [`DeadlineKind::describe`].
    Deadline { kind: DeadlineKind, text: String },
    /// The per-request turn budget was exhausted: `limit` turns are allowed and
    /// all of them ran, so turn `turn` was refused. (The count is deliberately
    /// reported as *refused*, matching the loop's pre-incremented counter —
    /// exactly `limit` turns ran; recon M2 confirmed the off-by-one itself is
    /// correct.)
    TurnCap { limit: usize, turn: usize },
}

impl BoundStopReason {
    /// Stable machine-readable tag for logs, journal lines and the session
    /// report. Never reworded without updating every consumer of the journal.
    pub fn code(&self) -> &'static str {
        match self {
            BoundStopReason::Deadline {
                kind: DeadlineKind::Stalled,
                ..
            } => "turn_watchdog",
            BoundStopReason::Deadline {
                kind: DeadlineKind::HardCap,
                ..
            } => "turn_hard_cap",
            BoundStopReason::TurnCap { .. } => "turn_cap",
        }
    }

    /// Short tag used in the `(Ready)` status line.
    ///
    /// The deadline branch is preserved byte-for-byte from the previous
    /// string-based selection so the existing bound status lines do not move.
    pub fn label(&self) -> &'static str {
        match self {
            BoundStopReason::Deadline { text, .. } => {
                if text.contains("hard cap") {
                    "turn hard cap"
                } else {
                    "turn watchdog"
                }
            }
            BoundStopReason::TurnCap { .. } => "turn budget",
        }
    }

    /// The user-visible (and journaled) reason. The turn-budget wording leads
    /// with [`BoundStopReason::code`] so the journaled line is machine-readable
    /// as well as human-readable, and names both the real limit and the turn
    /// count.
    pub fn text(&self) -> String {
        match self {
            BoundStopReason::Deadline { text, .. } => text.clone(),
            BoundStopReason::TurnCap { limit, turn } => format!(
                "{}: turn budget exhausted — the per-request limit is {limit} turns and all {limit} ran, so turn {turn} was refused. This request stopped at the turn budget, not because the work finished; split the goal or send a new instruction to continue.",
                self.code()
            ),
        }
    }
}

/// What the session loop reports back to its caller: the machine-readable
/// counterpart of the messages the operator was shown.
///
/// This is what makes a capped finish distinguishable from a clean one from
/// *outside* the UI — `run_session` cannot say anything at all, which is exactly
/// the residual defect t-074 fixes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionStopReport {
    /// Every session execution bound that cut a turn off, in the order it fired.
    /// Empty means no bound was hit: the turns finished on their own (user exit,
    /// abort or a clean end of turn).
    pub stops: Vec<BoundStopReason>,
}

impl SessionStopReport {
    /// Was the per-request turn budget exhausted at any point during the
    /// session? A capped finish must never be reportable as a clean finish.
    pub fn turn_cap_reached(&self) -> bool {
        self.stops
            .iter()
            .any(|stop| matches!(stop, BoundStopReason::TurnCap { .. }))
    }

    /// The stable machine-readable codes of every bound hit, in order.
    pub fn codes(&self) -> Vec<&'static str> {
        self.stops.iter().map(BoundStopReason::code).collect()
    }
}

// ── The Manager request budget (bug M7, gate t-064) ──────────────────────────
//
// [`ContextEngine::should_compact`] prices the **request**, i.e. transcript plus
// the serialized `ChatRequest.tools` payload
// ([`crate::manager::context::request_tokens`]). That schema term is only real if
// the live path declares the list it actually sends: an engine that never calls
// [`ContextEngine::set_tools`] budgets messages alone and therefore under-counts
// every Manager request by the size of the advertised schemas (thousands of
// tokens for the Manager default list). [`manager_wire_tools`] is the same
// assembly `src/llm/stream.rs::build_request` performs, so the charged schema and
// the wire schema are equal **by construction** — never a superset (which would
// compact transcript away too early) and never a subset (which would let an
// over-budget request through).

/// Build the session's Manager context engine, pre-charged with the exact tool
/// schemas the Manager advertises (bug M7, gate t-064).
///
/// The live loop constructs its engine **here** so the wiring is a single,
/// test-reachable line instead of an inline `ContextEngine::new` that silently
/// budgets messages only. The pinned goal is deliberately *not* set here: the
/// interactive goal does not exist until the first input is read, and
/// `run_session_with_bounds` pins it with [`ContextEngine::set_goal`] as before.
pub fn build_manager_context(cfg: &Config, system_prompt: String) -> ContextEngine {
    let mut ctx = ContextEngine::new(cfg.max_context_tokens);
    ctx.set_system_prompt(system_prompt);
    sync_manager_tool_schema(&mut ctx, cfg);
    ctx
}

/// Re-charge `ctx` with the tool schemas the Manager advertises **right now** and
/// return the charged schema token count (bug M7, gate t-064).
///
/// The advertised Manager list is not static: MCP servers boot lazily and
/// reconnect, and the advertised MCP view is policy-filtered per request
/// ([`crate::harness::allowed_mcp_tools`]). The session therefore re-declares the
/// list at every budget decision rather than trusting a one-time construction
/// charge — a stale charge is the same defect mirrored (compacting against a
/// schema the wire no longer carries).
pub fn sync_manager_tool_schema(ctx: &mut ContextEngine, cfg: &Config) -> usize {
    let previous = ctx.tool_schema_tokens();
    let advertised = manager_wire_tools(&cfg.orchestration.mcp_servers);
    ctx.set_tools(&advertised);
    let charged = ctx.tool_schema_tokens();
    if previous != charged {
        tracing::info!(
            "tool-schema budget {}: {previous} -> {charged} tokens across {} advertised tool(s)",
            if previous == 0 {
                "charged"
            } else {
                "re-priced"
            },
            advertised.len(),
        );
    }
    charged
}

/// Drive a full interactive session, dispatching events to `renderer`.
/// This is the production entry point: it applies [`SessionBounds::default()`]
/// (turn cap, per-turn wall-clock watchdog + hard cap, repeated-failure budget).
pub async fn run_session(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
) -> Result<()> {
    run_session_with_bounds(cfg, renderer, initial, manager, SessionBounds::default()).await
}

/// The live session loop with explicit execution bounds.
///
/// Bounds are injected rather than read from [`Config`] so the wall-clock and
/// failure-budget paths can be tested in milliseconds. `run_session` — the only
/// production caller — passes the defaults, so the shipped binary is bounded by
/// the constants in `src/manager/loop.rs` and nothing else.
///
/// Signature-preserving wrapper over [`run_session_with_bounds_report`], which
/// is where the machine-readable [`SessionStopReport`] is produced.
pub async fn run_session_with_bounds(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
    bounds: SessionBounds,
) -> Result<()> {
    run_session_with_bounds_report(cfg, renderer, initial, manager, bounds)
        .await
        .map(|_| ())
}

/// [`run_session_with_bounds`] plus a machine-readable account of **why** the
/// session stopped (t-074).
///
/// A session that ran out of turns used to be indistinguishable from one that
/// finished its work: the turn-cap exit emitted nothing at all. Every bound exit
/// now carries a [`BoundStopReason`], which is both shown through the session's
/// status channel and returned here, so a caller or a session summary can say
/// `report.turn_cap_reached()` instead of guessing from a silent stop.
pub async fn run_session_with_bounds_report(
    cfg: &Config,
    renderer: &mut dyn Renderer,
    initial: Option<String>,
    manager: Option<Arc<OrchestratorManager>>,
    bounds: SessionBounds,
) -> Result<SessionStopReport> {
    renderer.clear_abort();
    renderer.init()?;
    renderer.set_thinking_budgets(cfg);
    crate::debug_log::log_session_start(cfg, &cfg.ui_mode);

    let plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();

    // t-057: the plan gate is fail-closed and also reports a plan-integrity
    // warning. The prompt (and `tracing::warn!`) alone is not enough — the user
    // must see it too, so the warning is captured here and surfaced through the
    // session's existing warning channel once the transcript exists below.
    let (system, plan_warning) = load_system_prompt_with_plan_and_warning(cfg, &plan)?;
    // Gate t-064 (offender E): the live Manager engine is built with the exact
    // advertised tool list charged to the budget, so `should_compact()` measures
    // the request that is really sent (transcript **+** tool schemas).
    let mut ctx = build_manager_context(cfg, system.clone());

    let stats = manager
        .as_ref()
        .map(|m| Arc::clone(&m.stats))
        .unwrap_or_else(|| Arc::new(crate::harness::HarnessStats::new()));
    let default_mon = crate::config::MonitoringConfig::default();
    let mon_cfg = cfg.monitoring.as_ref().unwrap_or(&default_mon);
    let mut monitor =
        crate::harness::monitor::HarnessMonitor::new_with_config(stats.clone(), mon_cfg);

    let (status_tx, mut status_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let (event_tx, mut event_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
    let (steer_arb_tx, mut steer_arb_rx) = tokio::sync::mpsc::unbounded_channel::<SteerArbEvent>();
    let steering_history = Arc::new(std::sync::RwLock::new(Vec::<(String, String)>::new()));
    crate::orchestrator::set_status_sender(status_tx);
    crate::orchestrator::set_event_sender(event_tx);
    crate::orchestrator::set_steering_history(Arc::clone(&steering_history));

    let ui_transcript_path = plan.ui_transcript_path();
    let transcript_path = plan.transcript_path();

    let mut ui_transcript = UiTranscript::new();

    // t-057: an UNKNOWN plan state (unreadable / unparseable plan file) must reach
    // the user, not only the model. Same channel as every other plan-integrity
    // warning in this loop: a renderer `Status` event plus a `Status` record in the
    // UI transcript, via the session's single surfacing helper.
    if let Some(warning) = plan_warning {
        surface_plan_warning(
            &mut *renderer,
            &mut ui_transcript,
            &ui_transcript_path,
            warning,
        );
    }

    let mut steer_queue = Vec::<String>::new();
    let mut steer_abort_requested = false;
    let mut subagents = Vec::new();

    // H5 execution bounds, alive for the whole session (not per request):
    //  * `watchdog` — the per-turn wall-clock bound, re-armed at every turn
    //    (REQ-LOOP-002 wall-clock half),
    //  * `failures` — repeated-failure accounting keyed by plan task id (or by
    //    tool signature for direct tool calls).
    let mut watchdog = TurnWatchdog::new(bounds.turn_idle, bounds.turn_hard_cap);
    let mut failures = FailureBudget::new(bounds.failure_threshold);

    let client = ChatClient::from_config(cfg);
    let harness_stats = stats.clone();
    let stream_cfg = StreamConfig::from_config(cfg);

    drain_steer_arbitration_events_with_transcript(
        &mut steer_arb_rx,
        &mut *renderer,
        &mut steer_queue,
        &mut steer_abort_requested,
        Some(&mut subagents),
        Some(&mut ui_transcript),
    );

    let goal = match initial {
        Some(g) => {
            crate::debug_log::log_user_input("initial_argument", &g);
            g
        }
        None => loop {
            match renderer.read_input() {
                Some(line) => {
                    if is_abort_command(&line) {
                        crate::debug_log::log_user_input("command", &line);
                        renderer.request_user_exit();
                        renderer.shutdown();
                        return Ok(SessionStopReport::default());
                    }
                    if is_reset_command(&line) {
                        crate::debug_log::log_user_input("command", &line);
                        handle_reset_command(&plan, &mut *renderer, Some(&mut ctx));
                        ui_transcript.clear();
                        if let Ok(mut hist) = steering_history.write() {
                            hist.clear();
                        }
                        let _ = std::fs::remove_file(&ui_transcript_path);
                        continue;
                    }
                    let trimmed = line.trim();
                    if !trimmed.is_empty() {
                        crate::debug_log::log_user_input("interactive_goal", trimmed);
                        break trimmed.to_string();
                    }
                }
                None => {
                    renderer.shutdown();
                    return Ok(SessionStopReport::default());
                }
            }
        },
    };

    ctx.set_goal(goal.clone());
    ui_transcript.append(UiRecord::User { text: goal.clone() });
    let _ = ui_transcript.save(&ui_transcript_path);

    // t-074: every bound exit is collected here so the caller gets the same
    // distinction the operator gets on screen — a turn-cap stop is recorded and
    // reportable, never conflated with a clean finish.
    let mut session_stops: Vec<BoundStopReason> = Vec::new();

    crate::orchestrator::reset_cancellation();

    while !renderer.aborted() {
        let mut had_events = false;
        while let Ok(msg) = status_rx.try_recv() {
            renderer.on_event(&Event::Status(msg));
            had_events = true;
        }
        while let Ok(ev) = event_rx.try_recv() {
            if let Event::SteerResponse(ref text) = ev {
                ui_transcript.append(UiRecord::SteerResponse { text: text.clone() });
                let _ = ui_transcript.save(&ui_transcript_path);
            }
            renderer.on_event(&ev);
            had_events = true;
        }
        if had_events {
            let _ = renderer.flush();
        }

        drain_steer_arbitration_events_with_transcript(
            &mut steer_arb_rx,
            &mut *renderer,
            &mut steer_queue,
            &mut steer_abort_requested,
            Some(&mut subagents),
            Some(&mut ui_transcript),
        );
        drain_delegation_events_with_transcript(
            manager.as_deref(),
            &mut *renderer,
            &mut subagents,
            Some(&mut ui_transcript),
        );

        if let Some(steer) = renderer.poll_input() {
            if is_abort_command(&steer) {
                crate::debug_log::log_user_input("command", &steer);
                renderer.request_user_exit();
                break;
            }
            if is_reset_command(&steer) {
                crate::debug_log::log_user_input("command", &steer);
                handle_reset_command(&plan, &mut *renderer, Some(&mut ctx));
                ui_transcript.clear();
                if let Ok(mut hist) = steering_history.write() {
                    hist.clear();
                }
                let _ = std::fs::remove_file(&ui_transcript_path);
                continue;
            }
            if !steer.trim().is_empty() {
                crate::debug_log::log_user_input("midflight_steer", &steer);
                ui_transcript.append(UiRecord::User {
                    text: steer.clone(),
                });
                let _ = ui_transcript.save(&ui_transcript_path);
                if crate::orchestrator::has_active_workers()
                    || subagents.iter().any(|s| s.is_active)
                {
                    spawn_steer_arbitration(
                        &client,
                        harness_stats.clone(),
                        &goal,
                        &subagents,
                        steer,
                        &steer_arb_tx,
                        &mut *renderer,
                        Some(Arc::clone(&steering_history)),
                    );
                } else {
                    steer_queue.push(steer);
                }
            }
        }
        for steer in steer_queue.drain(..) {
            ctx.append(Message::User { content: steer });
        }

        let mut turn_count = 0;
        let mut nudge_count = 0;
        // Set when a session execution bound — a wall-clock deadline, or the
        // per-request turn budget — terminates this turn, carrying the
        // user-visible reason and its machine-readable code.
        let mut bound_stop_reason: Option<BoundStopReason> = None;
        loop {
            turn_count += 1;
            // t-074 (residual defect 2 of the manager-cluster gate t-067, live
            // analogue of recon finding M2): this exit used to break silently.
            // The operator saw a session stop responding after `MAX_TURNS` turns
            // with no status line, no journal record and nothing that separated
            // it from a normal end-of-turn finish. The reason is recorded here —
            // at the identical break point — and surfaced by the machinery the
            // wall-clock exits already use (after this loop): `tracing::warn!`,
            // a renderer `Message` + `Status` event, a matching `Status` record
            // in the UI transcript, and the machine-readable code the session
            // report returns.
            if turn_count > crate::manager::r#loop::MAX_TURNS {
                let reason = BoundStopReason::TurnCap {
                    limit: crate::manager::r#loop::MAX_TURNS,
                    turn: turn_count,
                };
                tracing::warn!(
                    session_stop_reason = reason.code(),
                    max_turns = crate::manager::r#loop::MAX_TURNS,
                    turn_count,
                    "turn budget exhausted: {}",
                    reason.text()
                );
                // t-063 invariant: this break happens at the **top** of the loop,
                // so it never passes the round-boundary repair below. The
                // transcript is already repaired by the previous round's
                // boundary, but the invariant is asserted here as well
                // (idempotent, and only surfaced if it actually changed
                // something), so a capped finish always leaves a valid chat
                // sequence on disk and on the next request.
                ensure_pairing_at_boundary(
                    "before turn-budget exit",
                    &mut ctx,
                    &mut *renderer,
                    &mut ui_transcript,
                    &ui_transcript_path,
                    &transcript_path,
                );
                bound_stop_reason = Some(reason);
                break;
            }
            if renderer.aborted() || renderer.user_exit_requested() {
                break;
            }
            // REQ-LOOP-002 (wall-clock half): bound ONE turn of the request.
            watchdog.rearm();

            // Status traffic from live workers/specialists is forward progress:
            // it refreshes the stalled-turn window so a legitimately long
            // delegation is never cut off by the watchdog.
            while let Ok(msg) = status_rx.try_recv() {
                renderer.on_event(&Event::Status(msg));
                watchdog.note_progress();
            }
            drain_steer_arbitration_events_with_transcript(
                &mut steer_arb_rx,
                &mut *renderer,
                &mut steer_queue,
                &mut steer_abort_requested,
                Some(&mut subagents),
                Some(&mut ui_transcript),
            );
            drain_delegation_events_with_transcript(
                manager.as_deref(),
                &mut *renderer,
                &mut subagents,
                Some(&mut ui_transcript),
            );
            renderer.flush()?;

            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }

            // Gate t-064 (offender E): re-price the advertised tool schema before
            // every budget decision. The advertised Manager list is not static
            // (MCP servers boot lazily / reconnect and the MCP view is
            // policy-filtered per request), so a one-time charge could compact
            // against a schema list the wire no longer carries.
            sync_manager_tool_schema(&mut ctx, cfg);
            if ctx.should_compact() {
                let outcome = ctx.compact();
                renderer.on_event(&Event::TokensIn(ctx.token_count()));
                surface_compaction_outcome(
                    &mut *renderer,
                    &mut ui_transcript,
                    &ui_transcript_path,
                    &outcome,
                );
            } else if ctx.should_advise_rebirth() {
                ctx.inject_rebirth_advisory();
                renderer.on_event(&Event::Status(
                    "context advisory: rebirth recommended (>= 80% budget)".to_string(),
                ));
            }

            renderer.reset_active_agent();
            renderer.on_event(&Event::TokensIn(ctx.token_count()));
            renderer.on_event(&Event::Status(format!("Running ({})", stream_cfg.model)));
            renderer.flush()?;

            // t-063 (manager gate item B): this is the exact line that turns the
            // transcript into the next provider request, so the tool-call pairing
            // invariant is enforced here as well — one idempotent call, with the
            // grammar owned by `context.rs`. A repair here means some earlier path
            // ended a turn with an assistant `tool_calls` entry and no result for
            // it; it is surfaced instead of silently rewritten.
            ensure_pairing_at_boundary(
                "before request",
                &mut ctx,
                &mut *renderer,
                &mut ui_transcript,
                &ui_transcript_path,
                &transcript_path,
            );

            let msgs = ctx.messages().to_vec();
            let mut bridge = RendererSink {
                renderer: &mut *renderer,
                steer_queue: &mut steer_queue,
                steer_abort_requested: &mut steer_abort_requested,
                arb_tx: &steer_arb_tx,
                arb_rx: &mut steer_arb_rx,
                client: &client,
                stats: harness_stats.clone(),
                goal: &goal,
                subagents: &subagents,
                plan: Some(&plan),
                ctx: Some(&mut ctx),
                steering_history: Some(Arc::clone(&steering_history)),
            };
            // REQ-LOOP-002 (wall-clock half): no await in the live loop may be
            // unbounded. A single backend call is bounded by the absolute
            // per-turn cap (`src/llm/client.rs` caps one stream at
            // `OVERALL_READ_TIMEOUT_SECS`; this cap is the backstop for a turn
            // that keeps re-entering the backend, and it makes an endless turn
            // impossible). On expiry: cancel in-flight work through the same
            // plumbing the abort paths use, and say why.
            let llm_budget = watchdog.time_to_hard_limit();
            let assistant = match tokio::time::timeout(
                llm_budget,
                chat_client_turn(&client, msgs, &stream_cfg, &mut bridge),
            )
            .await
            {
                Err(_elapsed) => {
                    let kind = watchdog.expired().unwrap_or(DeadlineKind::HardCap);
                    let reason = kind.describe(bounds.turn_idle, bounds.turn_hard_cap);
                    tracing::warn!(
                        elapsed_secs = watchdog.elapsed().as_secs(),
                        "turn bound reached during backend call: {reason}"
                    );
                    crate::orchestrator::cancel_all();
                    bound_stop_reason = Some(BoundStopReason::Deadline {
                        kind,
                        text: reason.clone(),
                    });
                    renderer.on_event(&Event::Message(format!("\n[{reason}]\n")));
                    renderer.on_event(&Event::Status(format!("{} (Ready)", kind.label())));
                    renderer.flush()?;
                    break;
                }
                Ok(Ok(m)) => m,
                Ok(Err(e)) => {
                    let category = classify_llm_error(&e);
                    tracing::warn!(
                        error = %format!("{e:#}"),
                        category = %category,
                        "LLM turn failed"
                    );
                    renderer.on_event(&Event::Message(format!(
                            "\n[Error] LLM backend call failed ({category}): {e:#}\n(Check that your LLM server is running at {} for model `{}`)",
                            client.backend_url(),
                            stream_cfg.model
                        )));
                    renderer.on_event(&Event::Status(format!("LLM error: {category} (Ready)")));
                    renderer.flush()?;
                    break;
                }
            };
            // A completed backend call is forward progress for the turn.
            watchdog.note_progress();

            if renderer.aborted()
                || renderer.user_exit_requested()
                || crate::orchestrator::is_globally_cancelled()
            {
                break;
            }

            let tool_calls = match &assistant {
                Message::Assistant {
                    tool_calls,
                    content,
                    ..
                } => {
                    if tool_calls.is_empty() {
                        if let Some(text) = content {
                            crate::debug_log::log_user_output("assistant_reply", text);
                            if cfg.enable_xml_rescue {
                                monitor.rescue_xml(text)
                            } else {
                                Vec::new()
                            }
                        } else {
                            Vec::new()
                        }
                    } else {
                        tool_calls.clone()
                    }
                }
                _ => Vec::new(),
            };

            ctx.append(assistant);
            let _ = ctx.save_transcript(&transcript_path);
            if let Some(Message::Assistant {
                content,
                reasoning_content,
                ..
            }) = ctx.messages().last()
            {
                let c = content.as_ref().filter(|s| !s.trim().is_empty()).cloned();
                let r = reasoning_content
                    .as_ref()
                    .filter(|s| !s.trim().is_empty())
                    .cloned();
                if c.is_some() || r.is_some() {
                    ui_transcript.append(UiRecord::Assistant {
                        content: c,
                        thinking: r,
                    });
                    let _ = ui_transcript.save(&ui_transcript_path);
                }
            }
            renderer.flush()?;

            if tool_calls.is_empty() {
                ctx.reset_consecutive_rebirths();
                sync_manager_tool_schema(&mut ctx, cfg);
                if ctx.should_compact() {
                    let outcome = ctx.compact();
                    let _ = ctx.save_transcript(&transcript_path);
                    renderer.on_event(&Event::TokensIn(ctx.token_count()));
                    surface_compaction_outcome(
                        &mut *renderer,
                        &mut ui_transcript,
                        &ui_transcript_path,
                        &outcome,
                    );
                } else if ctx.should_advise_rebirth() {
                    ctx.inject_rebirth_advisory();
                    let _ = ctx.save_transcript(&transcript_path);
                    renderer.on_event(&Event::Status(
                        "context advisory: rebirth recommended (>= 80% budget)".to_string(),
                    ));
                }
            }

            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }

            if tool_calls.is_empty() {
                let current_plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();
                // t-035a: read the plan fail-closed. A plan that cannot be read
                // or parsed is shown to the user through the warning channel
                // instead of being reported as "no pending work" (M8).
                let pending = match read_plan_gate(&current_plan) {
                    PlanGate::Read { pending, .. } => pending,
                    PlanGate::Unknown { warning } => {
                        surface_plan_warning(
                            &mut *renderer,
                            &mut ui_transcript,
                            &ui_transcript_path,
                            warning,
                        );
                        Vec::new()
                    }
                };
                if !steer_abort_requested
                    && !renderer.aborted()
                    && !crate::orchestrator::is_globally_cancelled()
                    && !pending.is_empty()
                    && nudge_count < 5
                {
                    nudge_count += 1;
                    let pending_str = pending.join(", ");
                    renderer.on_event(&Event::Status(format!(
                        "Auto-nudge ({nudge_count}/5): Plan incomplete (pending: {pending_str})"
                    )));
                    renderer.flush()?;
                    ctx.append(Message::User {
                        content: format!(
                            "(SYSTEM NOTICE: The execution plan is active on disk with pending tasks: [{pending_str}]. You are in the EXECUTING phase. You must call `delegate_task` to dispatch these pending tasks to specialists. Do NOT call `create_plan` again, and do NOT output conversational filler until all tasks are marked [x].)"
                        ),
                    });
                    continue;
                }
                break;
            }

            nudge_count = 0;

            let all_parallel = tool_calls.iter().all(|c| {
                c.function.name == crate::tool_names::TOOL_DELEGATE_TASK
                    || crate::manager::is_read_tool(&c.function.name)
            });

            if all_parallel && tool_calls.len() > 1 {
                ctx.reset_consecutive_rebirths();
                let mut handles = Vec::new();
                // Aligned index-for-index with `handles`: which unit of work each
                // joined result belongs to (H5 failure budget).
                let mut failure_keys: Vec<String> = Vec::new();
                for call in &tool_calls {
                    let name = call.function.name.clone();
                    let args_str = call.function.arguments.clone();
                    let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                        .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

                    let is_delegate = name == crate::tool_names::TOOL_DELEGATE_TASK;
                    let delegated_agent = if is_delegate {
                        args_val
                            .get("agent_name")
                            .and_then(serde_json::Value::as_str)
                            .and_then(|s| s.parse::<crate::agents::Agent>().ok())
                    } else {
                        None
                    };
                    let delegated_task = if is_delegate {
                        args_val
                            .get("task_id")
                            .and_then(serde_json::Value::as_str)
                            .and_then(crate::task_id::normalize_task_id)
                    } else {
                        None
                    };

                    // H5 failure budget: one identity per repeated unit of work —
                    // the plan task id for delegations, the tool signature for
                    // everything else.
                    let failure_key = failure_key_for(&name, &args_val, delegated_task.as_deref());
                    failure_keys.push(failure_key.clone());

                    if let Some(agent) = delegated_agent {
                        let task_prompt = args_val
                            .get("prompt")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            Some(task_prompt),
                            true,
                        );
                        renderer.set_subagents(subagents.clone());
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Started {
                                agent,
                                task: delegated_task.clone(),
                            },
                        ));
                    } else {
                        let display = format_tool_call_display(&name, &args_val);
                        renderer.on_event(&Event::ToolCall(display.clone()));
                        ui_transcript.append(UiRecord::ToolCall { display });
                        let _ = ui_transcript.save(&ui_transcript_path);
                    }

                    if call.is_malformed() {
                        let call_id = call.id.clone();
                        let err_msg = format!(
                            "ERROR: Invalid or truncated arguments for tool '{name}': output was cut off or contained unterminated JSON. Please reissue the tool call with complete, valid JSON arguments."
                        );
                        let handle = tokio::task::spawn_blocking(move || {
                            (
                                call_id,
                                delegated_agent,
                                delegated_task,
                                Ok(crate::harness::ToolResult::err(err_msg)),
                            )
                        });
                        handles.push(handle);
                        continue;
                    }

                    // H5 repeated-failure budget: once the same unit of work has
                    // failed `threshold` times in this session, do not dispatch it
                    // again. The refusal is reported as a normal tool error so the
                    // model sees the escalation instead of an endless retry loop.
                    if !failures.retry_allowed(&failure_key) {
                        let count = failures.count(&failure_key);
                        let err_msg = failure_refusal_message(&failure_key, count);
                        tracing::warn!(
                            failures = count,
                            "failure budget exhausted, refusing to retry {failure_key}"
                        );
                        let call_id = call.id.clone();
                        let handle = tokio::task::spawn_blocking(move || {
                            (
                                call_id,
                                delegated_agent,
                                delegated_task,
                                Ok(crate::harness::ToolResult::err(err_msg)),
                            )
                        });
                        handles.push(handle);
                        continue;
                    }

                    let intervention = monitor.observe_tool(&name, &args_val);
                    if matches!(
                        intervention,
                        crate::harness::monitor::Intervention::Block
                            | crate::harness::monitor::Intervention::Cut
                    ) {
                        let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                            format!(
                                "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls.",
                                call.function.name
                            )
                        });
                        tracing::warn!(
                            "Manager tool {} blocked by repetition detector",
                            call.function.name
                        );
                        let call_id = call.id.clone();
                        let handle = tokio::task::spawn_blocking(move || {
                            (
                                call_id,
                                delegated_agent,
                                delegated_task,
                                Ok(crate::harness::ToolResult::err(err_msg)),
                            )
                        });
                        handles.push(handle);
                        continue;
                    }

                    let invocation = crate::harness::ToolInvocation {
                        name: name.clone(),
                        arguments: args_val,
                    };

                    let call_id = call.id.clone();
                    let handle = tokio::task::spawn_blocking(move || {
                        (
                            call_id,
                            delegated_agent,
                            delegated_task,
                            crate::harness::dispatch_for(
                                &invocation,
                                crate::harness::ToolCaller::Manager,
                            ),
                        )
                    });
                    handles.push(handle);
                }
                // H2 (live hard-error path): a failing flush here propagates out of
                // `run_session`. The round has just been fanned out, so every handle
                // in `handles` is still in flight; abandoning them via `?` would
                // detach them and let delegated/parallel work keep running (and keep
                // its PTY process groups alive) after the session aborted. Cancel
                // through the same plumbing the abort paths use (`cancel_all` ->
                // global token, which `src/harness/pty.rs` polls to SIGKILL process
                // groups, plus `cancel_all_active_workers` for every registered
                // specialist) and abort the handles we still own.
                if let Err(e) = renderer.flush() {
                    crate::orchestrator::cancel_all();
                    for handle in handles.iter() {
                        handle.abort();
                    }
                    // t-063: this round has been fanned out but never joined, so
                    // the assistant `tool_calls` already in the transcript have no
                    // results at all. The hard error ends the session, but the
                    // transcript was written to disk turn by turn and can be
                    // reloaded — close the invariant and persist it before leaving.
                    if ctx.ensure_tool_call_pairs() {
                        let _ = ctx.save_transcript(&transcript_path);
                    }
                    return Err(e);
                }

                // Indexed so the still-unjoined tail of `handles` stays reachable
                // from the error path below.
                for idx in 0..handles.len() {
                    // A bound hit in an earlier join iteration already cancelled
                    // everything: drop out instead of re-joining the rest.
                    if bound_stop_reason.is_some() {
                        break;
                    }
                    let mut handle = &mut handles[idx];
                    // Attribute this join result to the unit of work that produced
                    // it (`failure_keys` is index-aligned with `handles`).
                    let failure_key = failure_keys[idx].clone();
                    if renderer.user_exit_requested() {
                        break;
                    }
                    let res = loop {
                        let mut had_events = false;
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            had_events = true;
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            if let Event::SteerResponse(ref text) = ev {
                                ui_transcript
                                    .append(UiRecord::SteerResponse { text: text.clone() });
                                let _ = ui_transcript.save(&ui_transcript_path);
                            }
                            renderer.on_event(&ev);
                            had_events = true;
                        }
                        if had_events {
                            let _ = renderer.flush();
                            watchdog.note_progress();
                        }
                        drain_steer_arbitration_events_with_transcript(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                            Some(&mut subagents),
                            Some(&mut ui_transcript),
                        );
                        drain_delegation_events_with_transcript(
                            manager.as_deref(),
                            &mut *renderer,
                            &mut subagents,
                            Some(&mut ui_transcript),
                        );
                        if renderer.user_exit_requested() {
                            crate::orchestrator::cancel_all();
                            break (
                                String::new(),
                                None,
                                None,
                                Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                    "aborted by user"
                                ))),
                            );
                        }
                        if renderer.aborted()
                            || steer_abort_requested
                            || crate::orchestrator::is_globally_cancelled()
                        {
                            crate::orchestrator::cancel_all();
                            let _ =
                                tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
                            break (
                                String::new(),
                                None,
                                None,
                                Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                    "aborted"
                                ))),
                            );
                        }
                        // H5 wall-clock bound: a turn that stops making progress
                        // (or exceeds the absolute cap) cancels the in-flight round
                        // through the same plumbing the abort paths use.
                        if let Some(kind) = watchdog.expired() {
                            let reason = kind.describe(bounds.turn_idle, bounds.turn_hard_cap);
                            tracing::warn!(
                                elapsed_secs = watchdog.elapsed().as_secs(),
                                "turn bound reached while awaiting parallel tool round: {reason}"
                            );
                            crate::orchestrator::cancel_all();
                            bound_stop_reason = Some(BoundStopReason::Deadline {
                                kind,
                                text: reason.clone(),
                            });
                            break (
                                String::new(),
                                None,
                                None,
                                Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                    "{reason}"
                                ))),
                            );
                        }
                        match tokio::time::timeout(join_poll_slice(&watchdog), &mut handle).await {
                            Ok(Ok(r)) => break r,
                            Ok(Err(e)) => {
                                break (
                                    String::new(),
                                    None,
                                    None,
                                    Err(crate::harness::ToolError::Execution(e.into())),
                                );
                            }
                            Err(_) => {
                                let mut had_events = false;
                                while let Ok(msg) = status_rx.try_recv() {
                                    renderer.on_event(&Event::Status(msg));
                                    had_events = true;
                                }
                                while let Ok(ev) = event_rx.try_recv() {
                                    if let Event::SteerResponse(ref text) = ev {
                                        ui_transcript
                                            .append(UiRecord::SteerResponse { text: text.clone() });
                                        let _ = ui_transcript.save(&ui_transcript_path);
                                    }
                                    renderer.on_event(&ev);
                                    had_events = true;
                                }
                                if had_events {
                                    let _ = renderer.flush();
                                    watchdog.note_progress();
                                }
                                drain_steer_arbitration_events_with_transcript(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
                                    &mut steer_abort_requested,
                                    Some(&mut subagents),
                                    Some(&mut ui_transcript),
                                );
                                drain_delegation_events_with_transcript(
                                    manager.as_deref(),
                                    &mut *renderer,
                                    &mut subagents,
                                    Some(&mut ui_transcript),
                                );
                                if let Some(input) = renderer.poll_input() {
                                    if is_abort_command(&input) {
                                        crate::debug_log::log_user_input("command", &input);
                                        renderer.request_user_exit();
                                    } else if is_reset_command(&input) {
                                        handle_reset_command(&plan, &mut *renderer, Some(&mut ctx));
                                        ui_transcript.clear();
                                        if let Ok(mut hist) = steering_history.write() {
                                            hist.clear();
                                        }
                                        let _ = std::fs::remove_file(&ui_transcript_path);
                                    } else if !input.trim().is_empty() {
                                        spawn_steer_arbitration(
                                            &client,
                                            harness_stats.clone(),
                                            &goal,
                                            &subagents,
                                            input,
                                            &steer_arb_tx,
                                            &mut *renderer,
                                            Some(Arc::clone(&steering_history)),
                                        );
                                    }
                                }
                                if renderer.user_exit_requested() {
                                    crate::orchestrator::cancel_all();
                                    break (
                                        String::new(),
                                        None,
                                        None,
                                        Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                            "aborted by user"
                                        ))),
                                    );
                                }
                            }
                        }
                    };

                    let (call_id, agent, task, tool_res) = res;
                    let (result_content, is_error) = match tool_res {
                        Ok(r) => (r.content, r.is_error),
                        Err(e) => (format!("ERROR: {e}"), true),
                    };
                    if let Some(ag) = agent {
                        update_subagent_lifecycle(&mut subagents, ag, task.clone(), None, false);
                        let tid = task.clone().unwrap_or_else(|| ag.to_string());
                        let clean_tid = task.as_deref().map(clean_task_id);
                        let sa_name = match &clean_tid {
                            Some(t) if !t.is_empty() => format!("{}-{t}", ag.as_str()),
                            _ => ag.as_str().to_string(),
                        };
                        if let Some(sa) =
                            find_subagent_mut(&mut subagents, &sa_name, clean_tid.as_deref())
                            && !is_error
                        {
                            sa.content = result_content.clone();
                            sa.context_tokens = count_text_tokens(&result_content);
                        }
                        if is_error {
                            let reason =
                                crate::ui::helpers::extract_failure_reason(&result_content);
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Failed {
                                    agent: ag,
                                    task,
                                    reason: Some(reason.clone()),
                                },
                            ));
                            ui_transcript.append(UiRecord::TaskFailed {
                                task_id: tid,
                                reason: Some(reason),
                            });
                        } else {
                            // REQ-PLAN-002 / t-035a: `is_error == false` is not
                            // completion evidence. The plan box flips only
                            // through the orchestrator's marker-gated entry
                            // point (`Plan::check_plan_on_deliverable`), so a
                            // deliverable that carries no valid completion
                            // marker leaves its plan task pending.
                            //
                            // The outcome is classified before anything is said:
                            // `handle_delegate_task` already ran `apply_check_off`
                            // for a marker-bearing deliverable, so an
                            // `AlreadyChecked` here is the normal, healthy path
                            // and must NOT be reported as a missing marker.
                            if let Some(ref tid_task) = task {
                                let outcome =
                                    marker_gated_check_off(&plan, Some(tid_task), &result_content);
                                if let Some(warning) = check_off_warning(Some(tid_task), &outcome) {
                                    surface_plan_warning(
                                        &mut *renderer,
                                        &mut ui_transcript,
                                        &ui_transcript_path,
                                        warning,
                                    );
                                }
                            }
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Completed { agent: ag, task },
                            ));
                            ui_transcript.append(UiRecord::TaskCompleted { task_id: tid });
                        }
                        let _ = ui_transcript.save(&ui_transcript_path);
                        renderer.set_subagents(subagents.clone());
                    } else {
                        renderer.on_event(&Event::ToolResult(result_content.clone()));
                        ui_transcript.append(UiRecord::ToolResult {
                            display: result_content.clone(),
                        });
                        let _ = ui_transcript.save(&ui_transcript_path);
                    }
                    drain_delegation_events_with_transcript(
                        manager.as_deref(),
                        &mut *renderer,
                        &mut subagents,
                        Some(&mut ui_transcript),
                    );

                    ctx.append(Message::Tool {
                        tool_call_id: call_id,
                        content: result_content,
                    });
                    let _ = ctx.save_transcript(&transcript_path);

                    // H5 repeated-failure accounting: an abort/cancel result is not
                    // the tool's own failure, so it must not consume the budget.
                    if is_error
                        && !(renderer.aborted()
                            || renderer.user_exit_requested()
                            || crate::orchestrator::is_globally_cancelled())
                    {
                        match failures.record(&failure_key) {
                            FailureVerdict::Continue { .. } => {}
                            FailureVerdict::Escalate { count } => {
                                let note = failure_escalation_note(&failure_key, count);
                                tracing::warn!(
                                    failures = count,
                                    "repeated failure escalation for {failure_key}"
                                );
                                ctx.append(Message::User {
                                    content: note.clone(),
                                });
                                renderer.on_event(&Event::Status(format!(
                                    "repeated failure: {} failed {count}x (strategy change required)",
                                    failure_label(&failure_key)
                                )));
                                ui_transcript.append(UiRecord::Status { text: note });
                                let _ = ui_transcript.save(&ui_transcript_path);
                            }
                        }
                    } else if !is_error {
                        failures.clear(&failure_key);
                    }
                    watchdog.note_progress();

                    // H2 (live hard-error path): flush only after the round result we
                    // just received has been appended to the context/transcript, so an
                    // error raised here cannot drop a completed deliverable. If the
                    // write does fail, cancel + abort the still-unjoined tail of the
                    // round instead of detaching it (see the note above the join loop).
                    if let Err(e) = renderer.flush() {
                        crate::orchestrator::cancel_all();
                        for handle in handles[idx + 1..].iter() {
                            handle.abort();
                        }
                        // t-063: the tail of the round is torn down here, so the
                        // calls after `idx` never get a `Message::Tool`. Repair and
                        // persist before propagating, so the transcript left on disk
                        // is a valid chat request.
                        if ctx.ensure_tool_call_pairs() {
                            let _ = ctx.save_transcript(&transcript_path);
                        }
                        return Err(e);
                    }

                    if renderer.user_exit_requested() {
                        break;
                    }
                }
                // H5: the break paths above (bound, abort, user exit) can leave the
                // rest of the round running. Cancel through the shared token and
                // abort every handle we still own; aborting a completed handle is a
                // no-op, so this only closes genuinely detached work.
                for handle in handles.iter() {
                    handle.abort();
                }
            } else {
                for call in &tool_calls {
                    if bound_stop_reason.is_some() {
                        break;
                    }
                    if renderer.aborted() || renderer.user_exit_requested() {
                        break;
                    }

                    let name = call.function.name.clone();
                    let args_str = call.function.arguments.clone();
                    let args_val = serde_json::from_str::<serde_json::Value>(&args_str)
                        .unwrap_or_else(|_| serde_json::Value::String(args_str.clone()));

                    let is_delegate = name == crate::tool_names::TOOL_DELEGATE_TASK;
                    let args_obj = args_val.as_object();
                    let delegated_agent = if is_delegate {
                        args_obj
                            .and_then(|o| o.get("agent_name"))
                            .and_then(serde_json::Value::as_str)
                            .and_then(|s| s.parse::<crate::agents::Agent>().ok())
                    } else {
                        None
                    };
                    let delegated_task = if is_delegate {
                        args_obj
                            .and_then(|o| o.get("task_id"))
                            .and_then(serde_json::Value::as_str)
                            .and_then(crate::task_id::normalize_task_id)
                    } else {
                        None
                    };

                    // H5 failure budget: one identity per repeated unit of work
                    // (plan task id for delegations, tool signature otherwise).
                    let failure_key = failure_key_for(&name, &args_val, delegated_task.as_deref());

                    if let Some(agent) = delegated_agent {
                        let task_prompt = args_obj
                            .and_then(|o| o.get("prompt"))
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            Some(task_prompt),
                            true,
                        );
                        renderer.set_subagents(subagents.clone());
                        renderer.on_event(&Event::Delegation(
                            crate::orchestrator::DelegationEvent::Started {
                                agent,
                                task: delegated_task.clone(),
                            },
                        ));
                    } else {
                        let display = format_tool_call_display(&name, &args_val);
                        renderer.on_event(&Event::ToolCall(display.clone()));
                        ui_transcript.append(UiRecord::ToolCall { display });
                        let _ = ui_transcript.save(&ui_transcript_path);
                    }
                    renderer.flush()?;

                    drain_delegation_events_with_transcript(
                        manager.as_deref(),
                        &mut *renderer,
                        &mut subagents,
                        Some(&mut ui_transcript),
                    );

                    if name == crate::tool_names::TOOL_REBIRTH {
                        renderer
                            .on_event(&Event::ToolCall(format_tool_call_display(&name, &args_val)));
                        renderer.flush()?;
                        let res = crate::harness::handle_rebirth(&mut ctx, &args_val);
                        match res {
                            Ok(r) => {
                                renderer.on_event(&Event::ToolResult(r.content.clone()));
                                if !r.is_error {
                                    ui_transcript.append(UiRecord::Status {
                                        text: format!("Rebirth checkpoint applied: {}", r.content),
                                    });
                                    let _ = ui_transcript.save(&ui_transcript_path);
                                    let _ = ctx.save_transcript(&transcript_path);
                                    renderer.on_event(&Event::TokensIn(ctx.token_count()));
                                    renderer.on_event(&Event::Status(
                                        "rebirth checkpoint applied".to_string(),
                                    ));
                                    let current_plan = manager
                                        .as_ref()
                                        .map(|m| m.plan.clone())
                                        .unwrap_or_default();
                                    // t-035a: same fail-closed plan read as the
                                    // auto-nudge gate — an unreadable/unparseable
                                    // plan is surfaced, never reported as
                                    // "nothing pending".
                                    let pending = match read_plan_gate(&current_plan) {
                                        PlanGate::Read { pending, .. } => pending,
                                        PlanGate::Unknown { warning } => {
                                            surface_plan_warning(
                                                &mut *renderer,
                                                &mut ui_transcript,
                                                &ui_transcript_path,
                                                warning,
                                            );
                                            Vec::new()
                                        }
                                    };
                                    let continuation_msg = if !pending.is_empty() {
                                        let pending_str = pending.join(", ");
                                        format!(
                                            "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. You have active pending tasks in your plan: [{pending_str}]. Do not call rebirth consecutively without making progress. Proceed immediately with delegating or executing these pending tasks.)"
                                        )
                                    } else {
                                        "(SYSTEM: Rebirth checkpoint accepted. Conversation history has been compacted. Do not call rebirth consecutively without making progress. Proceed immediately with delivering your final synthesis to the user.)".to_string()
                                    };
                                    ctx.append(Message::User {
                                        content: continuation_msg,
                                    });
                                    let _ = ctx.save_transcript(&transcript_path);
                                } else {
                                    ctx.append(Message::Tool {
                                        tool_call_id: call.id.clone(),
                                        content: format!("ERROR: {}", r.content),
                                    });
                                    let _ = ctx.save_transcript(&transcript_path);
                                }
                            }
                            Err(e) => {
                                renderer.on_event(&Event::ToolResult(format!("ERROR: {e}")));
                                ctx.append(Message::Tool {
                                    tool_call_id: call.id.clone(),
                                    content: format!("ERROR: {e}"),
                                });
                                let _ = ctx.save_transcript(&transcript_path);
                            }
                        }
                        renderer.flush()?;
                        continue;
                    }

                    ctx.reset_consecutive_rebirths();

                    if call.is_malformed() {
                        let err_msg = format!(
                            "ERROR: Invalid or truncated arguments for tool '{name}': output was cut off or contained unterminated JSON. Please reissue the tool call with complete, valid JSON arguments."
                        );
                        renderer.on_event(&Event::ToolResult(err_msg.clone()));
                        renderer.flush()?;
                        ctx.append(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: err_msg,
                        });
                        let _ = ctx.save_transcript(&transcript_path);
                        continue;
                    }

                    // H5 repeated-failure budget: refuse to re-dispatch work that
                    // has already failed the threshold number of times this session.
                    if !failures.retry_allowed(&failure_key) {
                        let count = failures.count(&failure_key);
                        let err_msg = failure_refusal_message(&failure_key, count);
                        tracing::warn!(
                            failures = count,
                            "failure budget exhausted, refusing to retry {failure_key}"
                        );
                        renderer.on_event(&Event::ToolResult(err_msg.clone()));
                        renderer.flush()?;
                        ctx.append(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: err_msg,
                        });
                        let _ = ctx.save_transcript(&transcript_path);
                        continue;
                    }
                    let intervention = monitor.observe_tool(&name, &args_val);
                    if matches!(
                        intervention,
                        crate::harness::monitor::Intervention::Block
                            | crate::harness::monitor::Intervention::Cut
                    ) {
                        let err_msg = monitor.intervention_error(intervention).unwrap_or_else(|| {
                            format!(
                                "ERROR: Tool repetition detected for '{}'. Do not repeat identical calls.",
                                call.function.name
                            )
                        });
                        tracing::warn!(
                            "Manager tool {} blocked by repetition detector",
                            call.function.name
                        );
                        renderer.on_event(&Event::ToolResult(format!("ERROR: {err_msg}")));
                        renderer.flush()?;
                        ctx.append(Message::Tool {
                            tool_call_id: call.id.clone(),
                            content: format!("ERROR: {err_msg}"),
                        });
                        let _ = ctx.save_transcript(&transcript_path);
                        continue;
                    }

                    let invocation = crate::harness::ToolInvocation {
                        name: name.clone(),
                        arguments: args_val,
                    };

                    let mut handle = tokio::task::spawn_blocking(move || {
                        crate::harness::dispatch_for(
                            &invocation,
                            crate::harness::ToolCaller::Manager,
                        )
                    });

                    let result = loop {
                        let mut had_events = false;
                        while let Ok(msg) = status_rx.try_recv() {
                            renderer.on_event(&Event::Status(msg));
                            had_events = true;
                        }
                        while let Ok(ev) = event_rx.try_recv() {
                            if let Event::SteerResponse(ref text) = ev {
                                ui_transcript
                                    .append(UiRecord::SteerResponse { text: text.clone() });
                                let _ = ui_transcript.save(&ui_transcript_path);
                            }
                            renderer.on_event(&ev);
                            had_events = true;
                        }
                        if had_events {
                            let _ = renderer.flush();
                            watchdog.note_progress();
                        }
                        drain_steer_arbitration_events_with_transcript(
                            &mut steer_arb_rx,
                            &mut *renderer,
                            &mut steer_queue,
                            &mut steer_abort_requested,
                            Some(&mut subagents),
                            Some(&mut ui_transcript),
                        );
                        drain_delegation_events_with_transcript(
                            manager.as_deref(),
                            &mut *renderer,
                            &mut subagents,
                            Some(&mut ui_transcript),
                        );
                        if renderer.user_exit_requested() {
                            crate::orchestrator::cancel_all();
                            break Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                "aborted by user"
                            )));
                        }
                        if renderer.aborted()
                            || steer_abort_requested
                            || crate::orchestrator::is_globally_cancelled()
                        {
                            crate::orchestrator::cancel_all();
                            let _ =
                                tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
                            break Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                "aborted"
                            )));
                        }
                        // H5 wall-clock bound: a turn that stops making progress
                        // (or exceeds the absolute cap) cancels the in-flight round
                        // through the same plumbing the abort paths use.
                        if let Some(kind) = watchdog.expired() {
                            let reason = kind.describe(bounds.turn_idle, bounds.turn_hard_cap);
                            tracing::warn!(
                                elapsed_secs = watchdog.elapsed().as_secs(),
                                "turn bound reached while awaiting sequential tool call: {reason}"
                            );
                            crate::orchestrator::cancel_all();
                            bound_stop_reason = Some(BoundStopReason::Deadline {
                                kind,
                                text: reason.clone(),
                            });
                            // Give the cancelled call a moment to tear its own work
                            // down (PTY process groups, sleep waits) instead of
                            // detaching it, mirroring the abort path above.
                            let _ =
                                tokio::time::timeout(Duration::from_millis(500), &mut handle).await;
                            break Err(crate::harness::ToolError::Execution(anyhow::anyhow!(
                                "{reason}"
                            )));
                        }
                        match tokio::time::timeout(join_poll_slice(&watchdog), &mut handle).await {
                            Ok(res) => {
                                break res
                                    .map_err(|e| crate::harness::ToolError::Execution(e.into()))
                                    .and_then(|r| r);
                            }
                            Err(_) => {
                                let mut had_events = false;
                                while let Ok(msg) = status_rx.try_recv() {
                                    renderer.on_event(&Event::Status(msg));
                                    had_events = true;
                                }
                                while let Ok(ev) = event_rx.try_recv() {
                                    if let Event::SteerResponse(ref text) = ev {
                                        ui_transcript
                                            .append(UiRecord::SteerResponse { text: text.clone() });
                                        let _ = ui_transcript.save(&ui_transcript_path);
                                    }
                                    renderer.on_event(&ev);
                                    had_events = true;
                                }
                                if had_events {
                                    let _ = renderer.flush();
                                    watchdog.note_progress();
                                }
                                drain_steer_arbitration_events_with_transcript(
                                    &mut steer_arb_rx,
                                    &mut *renderer,
                                    &mut steer_queue,
                                    &mut steer_abort_requested,
                                    Some(&mut subagents),
                                    Some(&mut ui_transcript),
                                );
                                drain_delegation_events_with_transcript(
                                    manager.as_deref(),
                                    &mut *renderer,
                                    &mut subagents,
                                    Some(&mut ui_transcript),
                                );
                                if let Some(input) = renderer.poll_input() {
                                    if is_abort_command(&input) {
                                        crate::debug_log::log_user_input("command", &input);
                                        renderer.request_user_exit();
                                    } else if is_reset_command(&input) {
                                        handle_reset_command(&plan, &mut *renderer, Some(&mut ctx));
                                        ui_transcript.clear();
                                        if let Ok(mut hist) = steering_history.write() {
                                            hist.clear();
                                        }
                                        let _ = std::fs::remove_file(&ui_transcript_path);
                                    } else if !input.trim().is_empty() {
                                        spawn_steer_arbitration(
                                            &client,
                                            harness_stats.clone(),
                                            &goal,
                                            &subagents,
                                            input,
                                            &steer_arb_tx,
                                            &mut *renderer,
                                            Some(Arc::clone(&steering_history)),
                                        );
                                    }
                                }
                                if renderer.user_exit_requested() {
                                    crate::orchestrator::cancel_all();
                                    break Err(crate::harness::ToolError::Execution(
                                        anyhow::anyhow!("aborted by user"),
                                    ));
                                }
                            }
                        }
                    };

                    let (result_content, is_error) = match result {
                        Ok(res) => (res.content, res.is_error),
                        Err(e) => (format!("ERROR: {e}"), true),
                    };
                    if let Some(agent) = delegated_agent {
                        update_subagent_lifecycle(
                            &mut subagents,
                            agent,
                            delegated_task.clone(),
                            None,
                            false,
                        );
                        let tid = delegated_task.clone().unwrap_or_else(|| agent.to_string());
                        let clean_tid = delegated_task.as_deref().map(clean_task_id);
                        let sa_name = match &clean_tid {
                            Some(t) if !t.is_empty() => format!("{}-{t}", agent.as_str()),
                            _ => agent.as_str().to_string(),
                        };
                        if let Some(sa) =
                            find_subagent_mut(&mut subagents, &sa_name, clean_tid.as_deref())
                            && !is_error
                        {
                            sa.content = result_content.clone();
                            sa.context_tokens = count_text_tokens(&result_content);
                        }
                        if is_error {
                            let reason =
                                crate::ui::helpers::extract_failure_reason(&result_content);
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Failed {
                                    agent,
                                    task: delegated_task,
                                    reason: Some(reason.clone()),
                                },
                            ));
                            ui_transcript.append(UiRecord::TaskFailed {
                                task_id: tid,
                                reason: Some(reason),
                            });
                        } else {
                            // REQ-PLAN-002 / t-035a: same marker gate as the
                            // parallel branch — a non-error result without a
                            // completion marker must never tick a plan box, and
                            // a box the orchestrator already ticked
                            // (`AlreadyChecked`) is never reported as a warning.
                            if let Some(ref tid) = delegated_task {
                                let outcome =
                                    marker_gated_check_off(&plan, Some(tid), &result_content);
                                if let Some(warning) = check_off_warning(Some(tid), &outcome) {
                                    surface_plan_warning(
                                        &mut *renderer,
                                        &mut ui_transcript,
                                        &ui_transcript_path,
                                        warning,
                                    );
                                }
                            }
                            renderer.on_event(&Event::Delegation(
                                crate::orchestrator::DelegationEvent::Completed {
                                    agent,
                                    task: delegated_task,
                                },
                            ));
                            ui_transcript.append(UiRecord::TaskCompleted { task_id: tid });
                        }
                        let _ = ui_transcript.save(&ui_transcript_path);
                        renderer.set_subagents(subagents.clone());
                    } else {
                        renderer.on_event(&Event::ToolResult(result_content.clone()));
                        ui_transcript.append(UiRecord::ToolResult {
                            display: result_content.clone(),
                        });
                        let _ = ui_transcript.save(&ui_transcript_path);
                    }
                    renderer.flush()?;

                    drain_delegation_events_with_transcript(
                        manager.as_deref(),
                        &mut *renderer,
                        &mut subagents,
                        Some(&mut ui_transcript),
                    );

                    ctx.append(Message::Tool {
                        tool_call_id: call.id.clone(),
                        content: result_content,
                    });
                    let _ = ctx.save_transcript(&transcript_path);

                    // H5 repeated-failure accounting: an abort/cancel result is not
                    // the tool's own failure, so it must not consume the budget.
                    if is_error
                        && !(renderer.aborted()
                            || renderer.user_exit_requested()
                            || crate::orchestrator::is_globally_cancelled())
                    {
                        match failures.record(&failure_key) {
                            FailureVerdict::Continue { .. } => {}
                            FailureVerdict::Escalate { count } => {
                                let note = failure_escalation_note(&failure_key, count);
                                tracing::warn!(
                                    failures = count,
                                    "repeated failure escalation for {failure_key}"
                                );
                                ctx.append(Message::User {
                                    content: note.clone(),
                                });
                                renderer.on_event(&Event::Status(format!(
                                    "repeated failure: {} failed {count}x (strategy change required)",
                                    failure_label(&failure_key)
                                )));
                                ui_transcript.append(UiRecord::Status { text: note });
                                let _ = ui_transcript.save(&ui_transcript_path);
                            }
                        }
                    } else if !is_error {
                        failures.clear(&failure_key);
                    }
                    watchdog.note_progress();
                    if renderer.user_exit_requested() {
                        break;
                    }
                }
            }

            // t-063 (manager gate item B): single repair boundary for the whole
            // round. Every way this round can end early leaves the transcript
            // half-paired — the assistant carrying **all** `tool_calls` was
            // appended before dispatch, and the skipped tail never got a
            // `Message::Tool`:
            //   * parallel join: `bound_stop_reason` break, `user_exit_requested`
            //     break, and the abort/cancel/bound breaks inside the join poll
            //     loop (the unjoined tail is only `handle.abort()`ed),
            //   * sequential dispatch: the `bound_stop_reason` /
            //     `renderer.aborted() || renderer.user_exit_requested()` breaks.
            // All of them fall through to this point *before* the
            // `bound_stop_reason` / `user_exit_requested` breaks below, which end
            // the turn and skip the compaction gate that used to be the only
            // caller of the repair. Repairing here (idempotent, grammar owned by
            // `ContextEngine::ensure_tool_call_pairs`) makes the transcript both
            // sendable on the next turn and valid on disk.
            ensure_pairing_at_boundary(
                "after tool round",
                &mut ctx,
                &mut *renderer,
                &mut ui_transcript,
                &ui_transcript_path,
                &transcript_path,
            );

            // H5: the round was cut off by an execution bound -> end the turn; the
            // reason is surfaced once, after the turn loop.
            if bound_stop_reason.is_some() {
                break;
            }
            if renderer.user_exit_requested() {
                break;
            }

            if !tool_calls.is_empty() {
                sync_manager_tool_schema(&mut ctx, cfg);
                if ctx.should_compact() {
                    let outcome = ctx.compact();
                    let _ = ctx.save_transcript(&transcript_path);
                    renderer.on_event(&Event::TokensIn(ctx.token_count()));
                    surface_compaction_outcome(
                        &mut *renderer,
                        &mut ui_transcript,
                        &ui_transcript_path,
                        &outcome,
                    );
                } else if ctx.should_advise_rebirth() {
                    ctx.inject_rebirth_advisory();
                    let _ = ctx.save_transcript(&transcript_path);
                    renderer.on_event(&Event::Status(
                        "context advisory: rebirth recommended (>= 80% budget)".to_string(),
                    ));
                }
            }

            let current_plan = manager.as_ref().map(|m| m.plan.clone()).unwrap_or_default();
            // t-035a: `is_complete()` is already fail-closed for an unreadable
            // plan, but the failure itself was invisible. Read the plan through
            // the same gate as everywhere else: the "all tasks COMPLETE" notice
            // is only ever injected for a plan that really was read and parsed,
            // and a plan that cannot be read/parsed is surfaced as a warning.
            // `complete` is judged from that single read, so the notice and the
            // warning can never disagree because two reads saw two files.
            match read_plan_gate(&current_plan) {
                PlanGate::Unknown { warning } => {
                    surface_plan_warning(
                        &mut *renderer,
                        &mut ui_transcript,
                        &ui_transcript_path,
                        warning,
                    );
                }
                PlanGate::Read { complete: true, .. } => {
                    ctx.append(Message::User {
                        content: "(SYSTEM NOTICE: All execution plan tasks are now COMPLETE [x]. Do NOT execute any more tools or re-delegate. Deliver your comprehensive final answer/synthesis to the user now.)".to_string(),
                    });
                    let _ = ctx.save_transcript(&transcript_path);
                }
                PlanGate::Read { .. } => {}
            }
        }

        // H5: make the bound user-visible exactly once per turn, keep the session
        // interactive (same reset pattern the steer-abort redirection uses), and
        // leave an explicit note in the context so the model cannot silently
        // repeat the approach that just hit the bound.
        //
        // t-074: the turn-budget exit travels this exact path too — one stop
        // vocabulary, one status channel — and every stop is additionally handed
        // to the caller through [`SessionStopReport`], so a capped finish can
        // never be reported as a clean one.
        if let Some(reason) = bound_stop_reason.take() {
            session_stops.push(reason.clone());
            let text = reason.text();
            let label = reason.label();
            tracing::warn!(
                session_stop_reason = reason.code(),
                "session turn loop stopped by an execution bound: {text}"
            );
            renderer.on_event(&Event::Message(format!("\n[Session bound] {text}\n")));
            renderer.on_event(&Event::Status(format!("{label} (Ready)")));
            ui_transcript.append(UiRecord::Status { text: text.clone() });
            let _ = ui_transcript.save(&ui_transcript_path);
            // Surface the teardown *before* the cancellation state is reset, so the
            // report and the cancelled state are observed together (and so no
            // renderer write can be interleaved with a half-reset session).
            renderer.flush()?;
            ctx.append(Message::User {
                content: format!(
                    "(SYSTEM NOTICE: the previous turn was terminated by a session execution bound: {text}. Do not repeat the same failing approach; report the blocker and ask for direction if no alternative strategy is available.)"
                ),
            });
            let _ = ctx.save_transcript(&transcript_path);
            if crate::orchestrator::is_globally_cancelled() {
                crate::orchestrator::reset_cancellation();
            }
        }
        drain_steer_arbitration_events_with_transcript(
            &mut steer_arb_rx,
            &mut *renderer,
            &mut steer_queue,
            &mut steer_abort_requested,
            Some(&mut subagents),
            Some(&mut ui_transcript),
        );
        if renderer.user_exit_requested() {
            break;
        }
        if steer_abort_requested || renderer.aborted() {
            if steer_abort_requested {
                // Steer arbitrator requested AbortImmediately / RejectPlan -> reset abort state, reset subagents, and start next turn immediately
                steer_abort_requested = false;
                renderer.clear_abort();
                crate::orchestrator::reset_cancellation();
                for s in subagents.iter_mut() {
                    if s.is_active {
                        s.is_active = false;
                        s.logs.push("[aborted by user]".to_string());
                    }
                }
                renderer.set_subagents(subagents.clone());
                for steer in steer_queue.drain(..) {
                    ctx.append(Message::User { content: steer });
                }
                let _ = ctx.save_transcript(&transcript_path);
                renderer.on_event(&Event::Status(
                    "Steering redirection: aborted current turn, starting next turn with updated context".to_string(),
                ));
                renderer.flush()?;
                continue;
            } else {
                break;
            }
        }

        // If user queued instructions during execution, start next turn immediately without blocking at read_input
        if !steer_queue.is_empty() {
            for steer in steer_queue.drain(..) {
                ctx.append(Message::User { content: steer });
            }
            let _ = ctx.save_transcript(&transcript_path);
            continue;
        }

        match renderer.read_input() {
            Some(line) => {
                if is_abort_command(&line) {
                    crate::debug_log::log_user_input("command", &line);
                    renderer.request_user_exit();
                    break;
                }
                if is_reset_command(&line) {
                    crate::debug_log::log_user_input("command", &line);
                    handle_reset_command(&plan, &mut *renderer, Some(&mut ctx));
                    ui_transcript.clear();
                    if let Ok(mut hist) = steering_history.write() {
                        hist.clear();
                    }
                    let _ = std::fs::remove_file(&ui_transcript_path);
                    continue;
                }
                if !line.trim().is_empty() {
                    crate::debug_log::log_user_input("interactive_input", &line);
                    ctx.append(Message::User {
                        content: line.clone(),
                    });
                    let _ = ctx.save_transcript(&transcript_path);
                    ui_transcript.append(UiRecord::User { text: line });
                    let _ = ui_transcript.save(&ui_transcript_path);
                } else {
                    continue;
                }
            }
            None => {
                break;
            }
        }
    }

    renderer.on_event(&Event::Done);
    renderer.flush().ok();
    renderer.shutdown();
    Ok(SessionStopReport {
        stops: session_stops,
    })
}

// ---------------------------------------------------------------------------
// H5 execution-bound helpers
// ---------------------------------------------------------------------------

/// Stable identity for a repeated unit of work inside one session.
///
/// Delegations are keyed by the plan task id they belong to, so re-delegating
/// the same task under a slightly different prompt still counts against the
/// budget. Everything else is keyed by tool name plus a truncated argument
/// signature, which is what makes "the same failing call over and over"
/// detectable without holding on to unbounded history.
fn failure_key_for(name: &str, args: &serde_json::Value, task_id: Option<&str>) -> String {
    if let Some(raw) = task_id {
        let task = clean_task_id(raw);
        if !task.is_empty() {
            return format!("task:{task}");
        }
    }
    let signature = crate::text_util::truncate_with_ellipsis(&args.to_string(), 120);
    format!("tool:{name}:{signature}")
}

/// User-facing rendering of a [`failure_key_for`] key.
fn failure_label(key: &str) -> String {
    match key.split_once(':') {
        Some(("task", task)) => format!("task {task}"),
        Some(("tool", rest)) => rest.split(':').next().unwrap_or(rest).to_string(),
        _ => key.to_string(),
    }
}

/// Escalation injected the moment a task/tool call crosses the failure budget:
/// it forbids an unchanged retry and names concrete strategies instead.
fn failure_escalation_note(key: &str, count: u32) -> String {
    format!(
        "(SYSTEM NOTICE: {} has failed {count} times in this session. Do NOT retry it unchanged: narrow the task, gather the missing context, split it into smaller tasks, or report it as a blocker and replan. Blindly repeating the same call is itself a failure.)",
        failure_label(key)
    )
}

/// What the loop reports back as the tool error once the budget is spent.
fn failure_refusal_message(key: &str, count: u32) -> String {
    format!(
        "ERROR: failure budget exhausted - {} has failed {count} times this session, so the session refuses to run it unchanged. Change strategy (different arguments, smaller scope, missing context gathered) or report the blocker to the user.",
        failure_label(key)
    )
}

// ---------------------------------------------------------------------------
// REQ-PLAN-002: plan check-off + plan-state helpers (shared by both live loops)
// ---------------------------------------------------------------------------

/// What the session is allowed to conclude about the on-disk execution plan.
///
/// t-035a: the session used to ask [`crate::manager::phase::Plan::pending_tasks`],
/// whose compatibility wrapper folds a read failure into an empty `Vec`. An
/// empty list is exactly what the auto-nudge and the completion notice read as
/// "nothing left to do", so a plan that cannot be read (or that cannot be parsed
/// into task lines) silently looked like a *finished* plan — bug M8 again, this
/// time at the UI boundary. This type keeps the two apart: `Unknown` must be
/// surfaced to the user and must never be taken as completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanGate {
    /// The plan was read and parsed by the single grammar owner
    /// ([`crate::plan_parse`]); `pending` holds its unchecked task ids and
    /// `complete` mirrors [`crate::manager::phase::Plan::is_complete`] computed
    /// from that **same** read (no second lock acquisition, no second, possibly
    /// different, read). An empty `pending` list here genuinely means "nothing
    /// pending" (or no plan file at all).
    Read {
        pending: Vec<String>,
        complete: bool,
    },
    /// The plan file exists but could not be read, or holds no parseable task
    /// line. `warning` is the user-visible text; the plan state is **unknown**,
    /// so the caller must show it and must not treat the plan as complete.
    Unknown { warning: String },
}

/// Read the plan the way the UI must judge it: fail-closed and loud.
///
/// Every branch is decided by the plan/grammar owners (`Plan::read`,
/// [`crate::plan_parse`]) — the session invents no plan grammar of its own.
pub fn read_plan_gate(plan: &crate::manager::phase::Plan) -> PlanGate {
    let content = match plan.read() {
        Ok(Some(content)) => content,
        // No plan file on disk: genuinely nothing pending (M8 keeps this apart
        // from a plan that exists but cannot be read).
        Ok(None) => {
            return PlanGate::Read {
                pending: Vec::new(),
                complete: false,
            };
        }
        Err(e) => {
            return PlanGate::Unknown {
                warning: format!(
                    "execution plan at {} could not be read: {e:#} — plan state is UNKNOWN, \
                     NOT 'all tasks done'",
                    plan.plan_path().display()
                ),
            };
        }
    };
    if content.trim().is_empty() {
        return PlanGate::Read {
            pending: Vec::new(),
            complete: false,
        };
    }
    // A non-empty plan that yields no task ids at all was not parsed: report it
    // instead of letting "no pending ids" masquerade as a completed plan.
    if crate::plan_parse::all_task_ids(&content).is_empty() {
        return PlanGate::Unknown {
            warning: format!(
                "execution plan at {} could not be parsed: it holds text but no \
                 `- [ ] [t-xxx]` task lines — plan state is UNKNOWN, NOT 'all tasks done'",
                plan.plan_path().display()
            ),
        };
    }
    PlanGate::Read {
        pending: crate::plan_parse::unchecked_task_ids(&content),
        // Same two conditions as `Plan::is_complete_unlocked`, evaluated on the
        // content that was just read.
        complete: !crate::plan_parse::has_unchecked_box(&content)
            && crate::plan_parse::has_checked_box(&content),
    }
}

/// Show a plan-integrity problem through the channel the session already uses
/// for warnings: a status event on the renderer plus a `Status` record in the UI
/// transcript (the exact pair the repeated-failure escalation emits), so the
/// user sees it live and after a restart.
fn surface_plan_warning(
    renderer: &mut dyn Renderer,
    ui_transcript: &mut UiTranscript,
    ui_transcript_path: &std::path::Path,
    warning: String,
) {
    tracing::warn!("{warning}");
    renderer.on_event(&Event::Status(warning.clone()));
    ui_transcript.append(UiRecord::Status { text: warning });
    let _ = ui_transcript.save(ui_transcript_path);
}

/// Wall-clock bound of one iteration of a tool-round join loop (t-063).
///
/// The join loops used to sleep a fixed 20 ms per iteration, so an idle-bound
/// teardown could be noticed up to a full slice late. The slice is now capped by
/// [`TurnWatchdog::time_to_idle_limit`] — the bound the loops honour is the one
/// the watchdog actually advertises. The `1 ms` floor keeps the loop from
/// spinning when the remaining budget is (sub-millisecond but) non-zero.
fn join_poll_slice(watchdog: &TurnWatchdog) -> Duration {
    const TOOL_JOIN_POLL_SLICE: Duration = Duration::from_millis(20);
    TOOL_JOIN_POLL_SLICE
        .min(watchdog.time_to_idle_limit())
        .max(Duration::from_millis(1))
}

/// Close the assistant `tool_calls` ↔ `Tool` pairing invariant on the live
/// transcript at a boundary that can end a turn early, persist it, and surface
/// it through the session's existing status channel — but only when the repair
/// actually changed something ([`ContextEngine::ensure_tool_call_pairs`] is
/// idempotent).
///
/// **Why the boundary is needed (manager gate item B):** the assistant message
/// carrying **all** `tool_calls` is appended before the round is dispatched, and
/// every early exit — an execution bound (`bound_stop_reason`), a renderer
/// abort, a steering abort, or a user exit — breaks out of the round before the
/// skipped tail of that round ever gets a `Message::Tool`. The transcript is
/// then re-sent verbatim on the next request, i.e. an assistant `tool_calls`
/// entry with no result: a provider 400. Nothing used to repair it, because the
/// repair lived only inside `compact()`, which is gated at > 90% utilization and
/// sits *after* those breaks.
///
/// `site` names the boundary in the user-visible status line so the repair is
/// attributable rather than silent.
fn ensure_pairing_at_boundary(
    site: &str,
    ctx: &mut ContextEngine,
    renderer: &mut dyn Renderer,
    ui_transcript: &mut UiTranscript,
    ui_transcript_path: &std::path::Path,
    transcript_path: &std::path::Path,
) {
    if !ctx.ensure_tool_call_pairs() {
        return;
    }
    // The repaired transcript is what the next request is built from and what a
    // rehydration would load, so it has to hit disk with the repair in it.
    let _ = ctx.save_transcript(transcript_path);
    let note = format!(
        "tool-call pairing repaired ({site}): missing tool results synthesized as `{ABORTED_TOOL_RESULT}`"
    );
    tracing::warn!("{note}");
    renderer.on_event(&Event::Status(note.clone()));
    ui_transcript.append(UiRecord::Status { text: note });
    let _ = ui_transcript.save(ui_transcript_path);
}

/// Report the outcome of an automatic compaction (t-063).
///
/// The live sites used to discard [`CompactionOutcome`] and print
/// `"context compacted"` unconditionally, which is a lie for
/// [`CompactionOutcome::TargetUnreachable`]: nothing reached the 70% target, the
/// transcript is still over budget, and the model is never told. Surfacing goes
/// through the session's one status channel (renderer `Status` event + matching
/// `Status` record in the UI transcript + `tracing::warn!`), the same channel
/// [`surface_plan_warning`] uses. Control flow is deliberately unchanged — the
/// session keeps running — but it must never claim a compaction that did not
/// happen.
fn surface_compaction_outcome(
    renderer: &mut dyn Renderer,
    ui_transcript: &mut UiTranscript,
    ui_transcript_path: &std::path::Path,
    outcome: &CompactionOutcome,
) {
    if outcome.succeeded() {
        renderer.on_event(&Event::Status("context compacted".to_string()));
        return;
    }
    let warning = format!(
        "(SYSTEM WARNING: automatic context compaction could not reach its target — removed {} messages / {} tokens and the transcript is still {} tokens against a {}-token target, i.e. above the compaction threshold. The pinned prefix alone exceeds the target; a rebirth or a shorter goal is required, the transcript is being sent over budget.)",
        outcome.messages_removed(),
        outcome.tokens_reclaimed(),
        outcome.final_tokens(),
        outcome.target(),
    );
    tracing::warn!("{warning}");
    renderer.on_event(&Event::Status(warning.clone()));
    ui_transcript.append(UiRecord::Status { text: warning });
    let _ = ui_transcript.save(ui_transcript_path);
}

/// Why a plan box did — or did not — flip, as decided by the single check-off
/// entry point [`crate::manager::phase::Plan::check_plan_on_deliverable`].
///
/// t-035a: a plain `bool` cannot tell the UI anything it is allowed to say out
/// loud. In particular `false` conflates "the box was already ticked by the
/// orchestrator's own marker-gated check-off" (harmless — `handle_delegate_task`
/// runs `apply_check_off` before the session ever sees the deliverable) with
/// "the deliverable carried no completion marker" (a real plan-integrity
/// warning). Printing a warning for the first case would be a lie, so the two
/// are separated here and only the genuinely suspicious outcomes are surfaced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckOffOutcome {
    /// A pending plan box was flipped to `[x]`.
    Flipped,
    /// The task line was already `[x]` before this call — the orchestrator's
    /// marker-gated check-off already recorded the completion.
    AlreadyChecked,
    /// No plan file on disk, or the bound id has no task line in it (a
    /// delegation that was never bound to a plan task).
    NotInPlan,
    /// The plan file is readable but the grammar owner finds **no task line at
    /// all** in it — the check-off cannot be located, and the plan state is
    /// unknown (never "all tasks done"). `plan_path` names the file to fix.
    Unparseable { plan_path: String },
    /// The deliverable carries no terminal marker at all: `is_error == false`
    /// was the only completion evidence offered.
    NoMarker,
    /// The deliverable carries a terminal verdict that is **not** a completion
    /// (`verdict` names it, e.g. `crate::markers::MARKER_FAILED`).
    NotComplete { verdict: String },
    /// The completion marker names a different task than the delegation bound —
    /// checking the bound box would tick another task's line.
    IdMismatch { bound: String, marker: String },
    /// The plan could not be read or written (IO).
    Io { detail: String },
    /// The check-off was refused by the owner for a documented fail-closed rule
    /// (e.g. a body-derived id that the deliverable never mentions as a token).
    Refused { detail: String },
}

/// REQ-PLAN-002 / t-035a: the single marker-gated check-off used by **both**
/// live session loops when a delegation comes back without an error.
///
/// It routes straight into [`crate::manager::phase::Plan::check_plan_on_deliverable`]
/// — the same entry point the orchestrator's `apply_check_off` uses — so a
/// deliverable that carries no terminal completion marker can never tick a plan
/// box, and `is_error == false` alone is never treated as completion. The marker
/// grammar and the task-id binding stay owned by [`crate::markers`] and
/// [`crate::plan_parse`]; the UI adds no parser of its own and makes no
/// decision of its own.
///
/// The one extra read below is **diagnostic only**: it captures the plan state
/// before the call so the outcome can be classified honestly (already checked vs
/// not in the plan). The authority to flip a box stays entirely in
/// `manager/phase.rs`.
#[must_use]
pub fn marker_gated_check_off(
    plan: &crate::manager::phase::Plan,
    task_id: Option<&str>,
    deliverable: &str,
) -> CheckOffOutcome {
    use crate::markers::MissionMarker;

    let bound = task_id
        .map(crate::task_id::normalize_task_id_ref)
        .filter(|t| !t.is_empty());
    // Diagnostic pre-state: does the plan even hold a line for this id, and was
    // it already `[x]` when we got here?
    let content = match plan.read() {
        Ok(content) => content,
        Err(e) => {
            return CheckOffOutcome::Io {
                detail: format!("{e:#}"),
            };
        }
    };
    let content: &str = content.as_deref().unwrap_or("");
    let line_present =
        bound.is_some_and(|tid| crate::plan_parse::find_task_line(content, tid).is_some());
    let already_checked = bound.is_some_and(|tid| crate::plan_parse::is_checked_task(content, tid));

    match plan.check_plan_on_deliverable(None, task_id, deliverable) {
        Ok(true) => return CheckOffOutcome::Flipped,
        Err(e) => {
            return CheckOffOutcome::Io {
                detail: format!("{e:#}"),
            };
        }
        Ok(false) => {}
    }
    // `Ok(false)`: classify why, reusing the owners' parsers.
    if already_checked {
        return CheckOffOutcome::AlreadyChecked;
    }
    if !content.trim().is_empty() && crate::plan_parse::all_task_ids(content).is_empty() {
        return CheckOffOutcome::Unparseable {
            plan_path: plan.plan_path().display().to_string(),
        };
    }
    if !line_present {
        return CheckOffOutcome::NotInPlan;
    }
    match MissionMarker::resolve(None, deliverable) {
        None => CheckOffOutcome::NoMarker,
        Some(MissionMarker::Complete { task_id: marker_id }) => {
            if let (Some(bound), Some(marker_id)) = (bound, marker_id.as_deref())
                && !crate::plan_parse::task_id_eq(bound, marker_id)
            {
                return CheckOffOutcome::IdMismatch {
                    bound: bound.to_owned(),
                    marker: marker_id.to_owned(),
                };
            }
            CheckOffOutcome::Refused {
                detail: "the owner declined this check-off; see the check_plan_on_deliverable \
                         warning for the fail-closed rule that applied"
                    .to_owned(),
            }
        }
        Some(marker) => CheckOffOutcome::NotComplete {
            verdict: match marker {
                MissionMarker::Replan { .. } => crate::markers::MARKER_REPLAN.to_owned(),
                _ => crate::markers::MARKER_FAILED.to_owned(),
            },
        },
    }
}

/// The user-visible warning for a [`CheckOffOutcome`], or `None` when the
/// outcome needs no announcement (a flip, an idempotent re-check, or a
/// delegation that was never bound to a plan task).
///
/// Marker words come only from [`crate::markers`].
#[must_use]
pub fn check_off_warning(task_id: Option<&str>, outcome: &CheckOffOutcome) -> Option<String> {
    let tid = task_id.unwrap_or("unbound");
    match outcome {
        CheckOffOutcome::Flipped | CheckOffOutcome::AlreadyChecked | CheckOffOutcome::NotInPlan => {
            None
        }
        CheckOffOutcome::Unparseable { plan_path } => Some(format!(
            "the execution plan at {plan_path} holds no parseable `- [ ] [t-id]` task line, so the \
             check-off for [{tid}] could not be applied — plan state is UNKNOWN, \
             NOT 'all tasks done'"
        )),
        CheckOffOutcome::NoMarker => Some(format!(
            "task {tid} returned without an error, but its deliverable carries no {} marker — \
             the plan box stays unchecked",
            crate::markers::MARKER_COMPLETE
        )),
        CheckOffOutcome::NotComplete { verdict } => Some(format!(
            "task {tid} returned a {verdict} verdict — its plan box stays unchecked and the task \
             must be re-delegated or replanned",
        )),
        CheckOffOutcome::IdMismatch { bound, marker } => Some(format!(
            "task {bound} came back with a {marker_name} marker for [{marker}] — a completion for \
             one task can never tick another task's box, so [{bound}] stays unchecked",
            marker_name = crate::markers::MARKER_COMPLETE
        )),
        CheckOffOutcome::Io { detail } => Some(format!(
            "plan check-off for [{tid}] failed: {detail} — plan state is UNKNOWN, the box was \
             NOT checked"
        )),
        CheckOffOutcome::Refused { detail } => Some(format!(
            "plan check-off for [{tid}] was refused: {detail} — the box stays unchecked"
        )),
    }
}

#[cfg(test)]
mod budget_tests {
    use super::{build_manager_context, sync_manager_tool_schema};
    use crate::config::Config;
    use crate::llm::stream::{StreamConfig, build_request};
    use crate::manager::context::{manager_wire_tools, tools_tokens};
    use crate::types::ToolDef;

    /// `ToolDef` has no `PartialEq`; the wire form is the only identity that
    /// matters for a schema charge, so the JSON is the comparison key.
    fn wire_signature(tools: &[ToolDef]) -> String {
        serde_json::to_string(tools).expect("tool definitions serialize")
    }

    #[test]
    fn charged_manager_schema_is_exactly_the_wire_schema() {
        let cfg = Config::default();
        let request = build_request(&StreamConfig::from_config(&cfg), Vec::new());
        let wire = request
            .tools
            .expect("the Manager always advertises its tool list");
        let charged = manager_wire_tools(&cfg.orchestration.mcp_servers);
        assert_eq!(
            wire_signature(&wire),
            wire_signature(&charged),
            "the schema the budget charges must equal the schema `build_request` puts on the wire"
        );
        assert!(
            tools_tokens(&charged) > 0,
            "the Manager advertised list is never schema-free"
        );
    }

    #[test]
    fn build_manager_context_charges_the_list_and_resync_is_idempotent() {
        let cfg = Config::default();
        let exact = tools_tokens(&manager_wire_tools(&cfg.orchestration.mcp_servers));
        assert!(exact > 0);

        let mut ctx = build_manager_context(&cfg, "You are the manager.".to_string());
        assert_eq!(ctx.tool_schema_tokens(), exact);
        assert!(ctx.request_token_count() > ctx.token_count());

        // A lost charge (stale MCP boot, a caller that wiped the list) is healed by
        // the per-decision re-declaration, and re-declaring twice is a no-op.
        ctx.set_tools(&[]);
        assert_eq!(ctx.tool_schema_tokens(), 0);
        assert_eq!(sync_manager_tool_schema(&mut ctx, &cfg), exact);
        assert_eq!(sync_manager_tool_schema(&mut ctx, &cfg), exact);
        assert_eq!(ctx.tool_schema_tokens(), exact);
    }
}
