//! Marmennill (marmel) — agentic coding assistant CLI entry point.

use anyhow::Result;
use marmennill::{config, harness, llm, manager, mcp, orchestrator, ui};

/// Command-line arguments accepted by the `marmel` binary.
#[derive(Debug, Default)]
struct CliArgs {
    /// Explicit path to a config file (overrides all lookup paths).
    config: Option<String>,
    /// Force raw (non-TUI) output mode.
    raw: bool,
    /// Detailed debug logging to debug.log.
    debug: bool,
    /// Rehydrate this run from the previous run's `.marmel/session/` bundle
    /// (equivalent to `MARMEL_RESUME=1` or `session_resume = true`):
    /// `ui::session::set_resume_request` records the decision and the session loop
    /// boots from the bundle, exiting with `session::EXIT_RESTORE_REFUSED` (3) when a
    /// bundle cannot be restored.
    resume: bool,
    /// Force a fresh start: overrides `session_resume = true` and
    /// `MARMEL_RESUME=1`. `--fresh` always wins; it is the escape hatch.
    fresh: bool,
    /// Preserve session and runtime files in `.marmel/` upon completion
    /// (by default, completed execution plans clean up Marmel's runtime files).
    keep_session: bool,
    prompt: Option<String>,
}

/// The exit signal that reached this process, spelled the way
/// `session::format::ProcessEnding::Signal` spells it: a **name**, never a bare number.
///
/// Nothing in `src/` used to look at signals at all, which is why a `kill` was observed
/// to be a hard kill: the process died in ~7 ms with the default disposition and left a
/// half-booted stdio MCP child behind, reparented to `ppid 1`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SignalArrival {
    /// SIGINT: a terminal interrupt, or `kill -INT`.
    Interrupt,
    /// SIGTERM: the usual "stop this process" signal.
    Terminate,
}

impl SignalArrival {
    /// The name carried by the log line (and by any `ProcessEnding::Signal` record).
    fn name(self) -> &'static str {
        match self {
            Self::Interrupt => "SIGINT",
            Self::Terminate => "SIGTERM",
        }
    }

    /// The signal number this arrival stands for (`man 7 signal`: SIGINT is 2,
    /// SIGTERM is 15).
    fn signum(self) -> i32 {
        match self {
            Self::Interrupt => 2,
            Self::Terminate => 15,
        }
    }

    /// The exit code a process that **chose** to stop on a signal should report: the
    /// shell convention `128 + signum`, i.e. 130 for SIGINT and 143 for SIGTERM. It is
    /// the honest verdict for a run that was cut short and therefore never produced a
    /// session outcome of its own.
    fn exit_code(self) -> i32 {
        128 + self.signum()
    }

    /// The marker carried in shared state: the signum (2 or 15), never 0, because 0 is
    /// the "no arrival yet" value of the atom that holds it.
    fn marker(self) -> u8 {
        self.signum() as u8
    }

    /// Reverse of [`Self::marker`]; `None` for the `0` sentinel and for anything else.
    fn from_marker(marker: u8) -> Option<Self> {
        match marker {
            2 => Some(Self::Interrupt),
            15 => Some(Self::Terminate),
            _ => None,
        }
    }
}

/// The half of the signal wiring that the watcher task and `main` share.
#[derive(Debug, Default)]
struct SignalState {
    /// How many exit signals have been seen: `0` means none, `1` means "request the
    /// exit", `2` and above mean "stop waiting and let the bounded teardown run".
    count: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// Which signal requested the exit (the **first** arrival), as the signum; `0` until
    /// an arrival records it.
    exit_marker: std::sync::Arc<std::sync::atomic::AtomicU8>,
    /// Woken by the first arrival only (`notify_one` stores its permit, so it is never
    /// missed even if it lands before anything awaits it).
    exit_request: std::sync::Arc<tokio::sync::Notify>,
    /// Woken by the second and every later arrival.
    force_request: std::sync::Arc<tokio::sync::Notify>,
}

impl SignalState {
    /// Record one arrival and wake the right waiter. Split out so the counting rules can
    /// be tested without an OS signal.
    fn observe(&self, arrival: SignalArrival) -> u32 {
        let seen = self
            .count
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            .saturating_add(1);
        if seen == 1 {
            self.exit_marker
                .store(arrival.marker(), std::sync::atomic::Ordering::Relaxed);
            self.exit_request.notify_one();
        } else {
            self.force_request.notify_one();
        }
        seen
    }

    /// Arrivals seen so far (a plain counter read, nothing to lock).
    fn arrival_count(&self) -> u32 {
        self.count.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// The arrival that requested the exit. The fallback only matters if the atom was
    /// written out of order; a signal that stopped this run always records it.
    fn exit_arrival(&self) -> SignalArrival {
        SignalArrival::from_marker(self.exit_marker.load(std::sync::atomic::Ordering::Relaxed))
            .unwrap_or(SignalArrival::Terminate)
    }
}

/// The installed watcher: shared state, plus the task that owns the OS handlers.
struct SignalWatch {
    state: std::sync::Arc<SignalState>,
    task: tokio::task::JoinHandle<()>,
}

impl SignalWatch {
    /// Stop watching for signals: the run has already decided how it ends, so a later
    /// arrival must not wake anything, and the installed handlers go back to the default
    /// disposition once the last stream handle is dropped.
    fn stop(&self) {
        self.task.abort();
    }
}

/// How long a run gets, after an exit signal, to reach **its own** quit path before the
/// forced teardown happens. It is the same 250 ms budget the MCP client sweep already
/// uses — the signal path must not invent a new, unbounded wait.
const SIGNAL_RUN_GRACE: std::time::Duration = std::time::Duration::from_millis(250);

/// What happened to the UI/session run: it either returned its own verdict, or an exit
/// signal stopped it.
#[derive(Debug)]
enum RunOutcome {
    /// The run finished on its own, `cleanTail` commit-on-exit included.
    Completed(Result<()>),
    /// An exit signal requested the exit: the run either never started (the signal
    /// landed during MCP startup) or was cut short after its bounded grace.
    Signalled,
}

/// Install the SIGINT/SIGTERM handlers and start the task that forwards arrivals into
/// `state`.
///
/// This lives **before** `McpManager::spawn_boot`, deliberately: the first stdio child is
/// spawned by the boot task, so a signal arriving while MCP is still booting is covered by
/// the handlers, and only the graceful teardown can reap that child. `tokio::signal::unix`
/// needs a runtime context to register its self-pipe handler, hence the one-shot
/// `block_on` on this thread: it returns as soon as the handlers exist, having awaited
/// nothing that can stall.
#[cfg(unix)]
fn install_exit_signal_watcher(rt: &tokio::runtime::Runtime) -> SignalWatch {
    let state = std::sync::Arc::new(SignalState::default());
    let task = rt.block_on(register_exit_signals(std::sync::Arc::clone(&state)));
    match task {
        Some(task) => SignalWatch { state, task },
        None => {
            tracing::warn!(
                "could not install the SIGINT/SIGTERM handlers: a signal falls back to the \
                 default disposition, which may orphan an MCP child that is still booting"
            );
            // Keep the shape of the contract: without handlers there is nothing to stop,
            // so the state simply never reports an arrival.
            SignalWatch {
                state,
                task: rt.spawn(async {}),
            }
        }
    }
}

#[cfg(unix)]
async fn register_exit_signals(
    state: std::sync::Arc<SignalState>,
) -> Option<tokio::task::JoinHandle<()>> {
    let mut interrupt =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt()).ok()?;
    let mut terminate =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()).ok()?;
    // The streams own the OS handlers, so they live (and stay registered) for as long as
    // this task does; `SignalWatch::stop` aborts the task and drops them.
    Some(tokio::spawn(async move {
        loop {
            let arrival = tokio::select! {
                Some(()) = interrupt.recv() => SignalArrival::Interrupt,
                Some(()) = terminate.recv() => SignalArrival::Terminate,
                // A stream that closed means the reactor side went away: nothing more to
                // watch, and every waiter below is already bounded.
                else => return,
            };
            let seen = state.observe(arrival);
            if seen == 1 {
                tracing::warn!(
                    "signal {} received: requesting exit and reaping MCP children",
                    arrival.name()
                );
            } else {
                tracing::warn!(
                    "signal {} received again (arrival {seen}): skipping the grace and \
                     forcing the bounded teardown",
                    arrival.name()
                );
            }
        }
    }))
}

/// Install the exit-signal watcher on a platform without `SIGINT`/`SIGTERM`.
#[cfg(not(unix))]
fn install_exit_signal_watcher(rt: &tokio::runtime::Runtime) -> SignalWatch {
    let state = std::sync::Arc::new(SignalState::default());
    let task = rt.block_on(register_console_signals(std::sync::Arc::clone(&state)));
    SignalWatch { state, task }
}

#[cfg(not(unix))]
async fn register_console_signals(
    state: std::sync::Arc<SignalState>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        loop {
            // `ctrl_c` re-arms its own handler per call; an error means the console is
            // gone, so the loop ends rather than spinning.
            if tokio::signal::ctrl_c().await.is_err() {
                return;
            }
            state.observe(SignalArrival::Interrupt);
        }
    })
}

/// Compose the UI future and the exit-signal future **on the runtime**, so that whichever
/// way the run ends, control reaches the same teardown lines in `main`
/// (`cancel_all()` → `shutdown_boot_task()` → bounded `shutdown()` → `shutdown_timeout`).
///
/// The documented semantics, including the raw/TUI caveat:
///
/// * While the terminal is in raw mode the TUI owns Ctrl-C: the terminal's `ISIG` is off,
///   so a Ctrl-C key press is already a crossterm event handled by
///   `ui::tui::TuiRenderer::request_user_exit` — no signal is generated and this handler
///   never sees it. This is left untouched, so interactive behaviour does not change.
/// * A real SIGINT/SIGTERM arriving **during** the run (an external `kill`, a service
///   stop, job control) is treated as a process-level exit request. It first replays what
///   the UI's own quit path does — `orchestrator::cancel_all()`, exactly what
///   `request_user_exit` calls — so the session loop's abort/commit path (the `T-6`/`T-7`
///   triggers) gets its bounded chance to run, and only then is the run abandoned. That
///   keeps the UI's own bundle commit first, and the bounded teardown second.
/// * A signal that arrives **before** the run starts (e.g. during MCP startup) means "do
///   not start this run at all".
///
/// The wait for the run's own quit path is bounded by [`SIGNAL_RUN_GRACE`]; a second
/// arrival stops that wait immediately, so the twice-over case cannot deadlock.
async fn run_reacting_to_exit_signal<R>(
    state: &SignalState,
    grace: std::time::Duration,
    run: R,
) -> RunOutcome
where
    R: std::future::Future<Output = Result<()>>,
{
    if state.arrival_count() > 0 {
        tracing::warn!("exit signal received before the run started: skipping the session run");
        return RunOutcome::Signalled;
    }
    tokio::pin!(run);
    tokio::select! {
        outcome = &mut run => return RunOutcome::Completed(outcome),
        _ = state.exit_request.notified() => {}
    }
    // First arrival: let the run wind down on its own terms for as long as the bound
    // allows. `cancel_all()` is the same call the UI quit path makes.
    orchestrator::cancel_all();
    let grace = tokio::time::timeout(grace, &mut run);
    tokio::pin!(grace);
    tokio::select! {
        outcome = grace => {
            match outcome {
                Ok(inner) => return RunOutcome::Completed(inner),
                Err(_elapsed) => {}
            }
        }
        _ = state.force_request.notified() => {}
    }
    tracing::warn!("run not finished within its grace: forcing the bounded teardown");
    RunOutcome::Signalled
}

fn main() -> Result<()> {
    let mut raw_args = std::env::args().skip(1);
    if is_sandbox_reentry(raw_args.next().as_deref()) {
        return run_sandbox_reentry(&mut raw_args);
    }

    let args = parse_args();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    harness::set_workspace_root(&cwd);

    let mut cfg = config::load(args.config.as_deref())?;
    if args.debug {
        cfg.debug = true;
    }
    // Session flags are validated before anything is started: no config is
    // published, no runtime is built, no bundle is touched. With neither flag
    // present this is a no-op and the run behaves exactly as before.
    if let Some(message) = session_flag_error(&args, &cfg) {
        eprintln!("error: {message}");
        std::process::exit(2);
    }
    config::set_active(cfg.clone());
    // Hand the CLI's resume decision to the module that owns the session loop. The
    // precedence `--fresh` > `--resume` > `MARMEL_RESUME` / `session_resume` is
    // documented at `ui::session::explicit_resume_requested`, and the contradictory
    // combinations are rejected above (`exit 2`). With neither flag this records
    // `false`/`false`: the resume-by-default decision belongs to
    // `ui::session::resume_intent`, which also weighs the execution plan on disk.
    ui::session::set_resume_request(args.resume, args.fresh);
    manager::phase::set_keep_session(args.keep_session);

    if cfg.debug {
        let ws = harness::workspace::Workspace::new();
        let debug_log_path = ws
            .as_ref()
            .map(|w| w.root().join("debug.log"))
            .unwrap_or_else(|_| std::path::PathBuf::from("debug.log"));
        marmennill::debug_log::init(Some(debug_log_path));
    }

    let use_raw = args.raw || cfg.ui_mode == "raw" || !stdout_is_terminal();
    setup_panic_hook(use_raw);

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // Exit signals are wired up **here, before MCP is started**: the first stdio child is
    // spawned by the boot task below, so a `SIGINT`/`SIGTERM` that lands while MCP is still
    // booting has to be covered by the handlers, or the child is orphaned. Nothing is
    // awaited that could stall — the call returns as soon as the handlers exist.
    //
    // The raw/TUI caveat, and the behaviour chosen for it: while the terminal is in raw
    // mode the TUI owns Ctrl-C (`ISIG` is off, so a Ctrl-C press is a crossterm key event
    // that `ui::tui::TuiRenderer::request_user_exit` already handles, and no signal is
    // generated at all) — interactive behaviour is untouched. A *real* signal arriving
    // during the run is therefore treated as a process-level exit request: it first
    // replays the UI's own quit path (`orchestrator::cancel_all()`, the very call
    // `request_user_exit` makes) so the session loop gets its bounded chance to wind down
    // and commit, and only then runs the same bounded teardown this function already runs.
    let signal = install_exit_signal_watcher(&rt);

    // One honest line about the shared HTTP client before anything needs it — the
    // MCP boot, the Manager and every session turn below all talk to a backend
    // through it. A host with no TLS trust store makes that client impossible to
    // build; the client then carries the verdict instead of panicking, so this is
    // a warning about a run whose every LLM turn will fail, not a new verdict:
    // the process keeps the exit code its session outcome produces.
    report_http_client_verdict(use_raw);

    // MCP is started, never waited on: **startup returns without waiting for MCP.**
    // `spawn_boot` hands back the still-filling manager without awaiting anything, and the
    // gate is registered in the same breath, so the harness slot exists before the first
    // dispatch. That first gate is a frozen snapshot of an empty registry, hence its
    // `allowed` set is empty at this point — expected, not a regression: MCP tools are
    // simply undispatchable until the verdict lands.
    //
    // The readiness continuation lives **here**, in main.rs, keeping the invariant that
    // main.rs is the only production writer of the global gate slot: it awaits the boot
    // verdict and, on `Ready`, re-registers the *same* manager, at which point
    // `build_mcp_gate` recomputes `allowed`/`rejected` from the now-populated tool
    // snapshot. On `Failed` (not one configured server came up) nothing is re-registered —
    // the empty gate and every built-in name are untouched — and the reason is logged.
    //
    // A `Failed` payload comes in two shapes and they are logged differently, because the
    // catch-all wording is wrong for the common one: the payload an **aborted boot task**
    // publishes (`mcp::McpBootStatus::BOOT_ABORT_REASON`) only means "this run ended while
    // the boot was still in flight" — the usual shape when the user simply quit. That
    // verdict is reported as *startup still in progress*, together with the count of
    // servers that **did** come up; only a real per-server failure list is reported as a
    // boot failure.
    //
    // `spawn_boot` already returns `Arc<Self>`, so no extra allocation is needed; the Arc
    // is kept locally for the exit-path teardown below.
    let mcp_manager: Option<std::sync::Arc<mcp::McpManager>> = if cfg.mcp_servers.is_empty() {
        None
    } else {
        let mgr = mcp::McpManager::spawn_boot(rt.handle(), &cfg.mcp_servers);
        harness::set_mcp_manager(std::sync::Arc::clone(&mgr));
        let watcher = std::sync::Arc::clone(&mgr);
        // How many servers were configured — captured as a plain number so the
        // continuation can state `N of M` without moving `cfg` into the task.
        let configured = cfg.mcp_servers.len();
        rt.spawn(async move {
            match watcher.await_ready().await {
                mcp::McpBootStatus::Ready => harness::set_mcp_manager(watcher),
                mcp::McpBootStatus::Failed(reason) => {
                    let came_up = watcher.booted_server_count();
                    if mcp::McpBootStatus::is_boot_abort_reason(&reason) {
                        // The boot was cut short by this run ending: not a failure
                        // diagnosis, and not necessarily an empty registry.
                        tracing::warn!(
                            "MCP startup still in progress at exit — {came_up} of \
                             {configured} server(s) came up, the rest were still starting"
                        );
                    } else {
                        tracing::warn!(
                            "MCP boot failed: {reason} — no MCP server came up \
                             ({configured} configured)"
                        );
                    }
                }
                // `await_ready` only publishes a terminal verdict while the manager lives;
                // `Booting` here means the publisher was closed mid-boot, which the local
                // Arc cannot cause. Treat it as "nothing to re-register".
                mcp::McpBootStatus::Booting => {}
            }
        });
        Some(mgr)
    };

    let manager = Some(boot_manager(&cfg));

    // The UI future and the exit-signal future are composed **on the runtime**, so a
    // signal cannot skip the teardown below: whichever way this returns, the same
    // `cancel_all()` → MCP reap → `shutdown_timeout()` lines run next, and they run
    // bounded.
    let outcome = if use_raw {
        let run = ui::raw::run(&cfg, args.prompt, manager);
        rt.block_on(run_reacting_to_exit_signal(
            &signal.state,
            SIGNAL_RUN_GRACE,
            run,
        ))
    } else {
        let run = ui::tui::run(&cfg, args.prompt, manager);
        rt.block_on(run_reacting_to_exit_signal(
            &signal.state,
            SIGNAL_RUN_GRACE,
            run,
        ))
    };

    // The run has decided how it ends, so stop watching: a later arrival must not wake
    // anything. Aborting the task drops the stream handles, and the last handle for a
    // signal is what puts the OS handler back to the disposition it had before this
    // process installed one.
    signal.stop();
    if matches!(outcome, RunOutcome::Signalled) {
        // The abandoned UI future never reached its own leave-alt-screen / raw-mode
        // restore, so the terminal is put back before the bounded teardown below runs.
        ui::restore();
    }

    orchestrator::cancel_all();
    // Best-effort MCP teardown, bounded so it can never stall the exit. Two steps, in this
    // order: `shutdown_boot_task()` aborts the background boot where it stands (dropping a
    // half-booted stdio child, whose `kill_on_drop(true)` then reaps it), and
    // `McpManager::shutdown()` kills every client that did come up. Without this the only
    // thing stopping the runtime is `shutdown_timeout`, which forgets tasks and connection
    // tasks without ever reaping the children — i.e. every stdio MCP server would be
    // orphaned and outlive this process.
    if let Some(mgr) = mcp_manager.as_ref() {
        mgr.shutdown_boot_task();
        let mgr = std::sync::Arc::clone(mgr);
        let _ = rt.block_on(async {
            tokio::time::timeout(std::time::Duration::from_millis(250), mgr.shutdown()).await
        });
    }
    rt.shutdown_timeout(std::time::Duration::from_millis(300));
    // A run stopped by an exit signal never produced a session verdict of its own, so the
    // verdict it reports is the shell convention `128 + signum`: 130 for SIGINT, 143 for
    // SIGTERM, for the signal that actually asked this process to stop. This sits **after**
    // the teardown above, so choosing to stop this way never skipped a reap.
    let res = match outcome {
        RunOutcome::Completed(result) => result,
        RunOutcome::Signalled => {
            let arrival = signal.state.exit_arrival();
            eprintln!(
                "run stopped by {}: exiting {} (MCP children reaped; the session bundle is \
                 left exactly as it was last committed)",
                arrival.name(),
                arrival.exit_code()
            );
            std::process::exit(arrival.exit_code())
        }
    };
    // A **refused** `--resume` is a first-class verdict, not a runtime error:
    // `main() -> Result<()>` can only ever exit `1`, so the typed
    // `session::RestoreRefused` returned by the boot pass is mapped here — after the
    // teardown above has restored the screen and cancelled every worker — onto the
    // named constant `session::EXIT_RESTORE_REFUSED` (3). The refusal itself (code,
    // member, byte_len/fingerprint pair, bundle path) was already printed through the
    // session's renderer, journal and log; it is repeated to stderr because the TUI
    // status line is already torn down at this point.
    if let Err(err) = &res
        && err
            .downcast_ref::<marmennill::session::restore::RestoreRefused>()
            .is_some()
    {
        eprintln!("{err}");
        std::process::exit(marmennill::session::EXIT_RESTORE_REFUSED);
    }
    if res.is_ok() && !manager::phase::is_keep_session() {
        let plan = manager::phase::Plan::default();
        let _ = plan.cleanup_completed();
    }
    res
}

/// `true` when argv starts with the sandbox re-entry sentinel.
///
/// The sentinel comes from [`harness::pty::SANDBOX_EXEC_ARG`], the same constant the
/// spawn side uses, so the two ends of the protocol can never drift apart.
fn is_sandbox_reentry(first: Option<&str>) -> bool {
    first == Some(harness::pty::SANDBOX_EXEC_ARG)
}

/// The sandbox re-entry path: apply the backend this host has — Landlock on Linux, Seatbelt
/// on macOS, the Windows AppContainer on Windows — to this process, then run the pending
/// command in that confinement — **only if enforcement was actually established**. The order is
/// load-bearing: resolve the cwd canonically ([`canonical_sandbox_cwd`]); honour the
/// explicit opt-out ([`harness::pty::SANDBOX_OPTOUT_ENV`]) *before* any enforcement call,
/// saying so out loud ([`harness::sandbox::optout_warning`]); otherwise attempt
/// enforcement; then ask [`harness::sandbox::reentry_action`], which on
/// [`harness::sandbox::ReentryAction::Refuse`] never runs the command unsandboxed:
/// stderr gets the reason; exit code [`harness::sandbox::SANDBOX_REFUSE_EXIT_CODE`] (non-zero).
///
/// The two halves of the Windows backend meet here: the policy pass parks the prepared
/// AppContainer setup, and the confined launch inside [`exec_pending_command`] picks it up,
/// launches the pending command inside that AppContainer and exits with the child's exit code.
/// A refusal releases the parked setup instead, so a run that never launched anything does not
/// leave the workspace DACLs extended.
fn run_sandbox_reentry(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let cwd_raw = args.next().unwrap_or_else(|| ".".to_string());
    let command = args.next().unwrap_or_default();
    let cwd = canonical_sandbox_cwd(&cwd_raw);

    // The extra read-only paths configured by the operator arrive through the
    // environment: this process is the re-entry child, and it never loads a config
    // file, so the spawning process published them in the variable named by
    // harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV. Absent means the built-in
    // allow-list only. They are read once, here, and handed to the enforcement call
    // below, which turns them into read-only rules for whichever backend runs here.
    let extra_readonly_paths = harness::sandbox::extra_readonly_paths_from_env();
    tracing::debug!(
        count = extra_readonly_paths.len(),
        "sandbox re-entry: extra read-only paths received from the spawning process"
    );

    let resolved_exe = harness::pty::resolved_exe_path();
    let decision = harness::pty::should_apply_sandbox(&harness::pty::SandboxInputs {
        backend_supported: harness::sandbox::backend_supports_current_platform(),
        // This file *is* the bin target that implements the re-entry protocol.
        test_harness_exe: false,
        resolved_exe: resolved_exe.as_deref(),
        opt_out: harness::sandbox::opt_out_from_env(),
    });
    harness::pty::log_sandbox_decision(&decision);

    let enforcement = match &decision {
        // The only case allowed to skip enforcement, and it is never silent: the
        // logging subscriber may not exist yet on this path, so stderr is the channel
        // the operator actually sees.
        harness::pty::SandboxDecision::Skip {
            skip: harness::pty::SandboxSkip::OptOut { setting },
            ..
        } => {
            eprintln!("{}", harness::sandbox::optout_warning(setting));
            tracing::warn!("sandbox disabled by {setting}: the command runs unsandboxed");
            harness::sandbox::Enforcement::Skipped {
                setting: setting.clone(),
            }
        }
        _ => match harness::sandbox::apply_sandbox_with_extra_readonly_paths(
            &cwd,
            &extra_readonly_paths,
        ) {
            Ok(()) => harness::sandbox::Enforcement::Established,
            Err(err) => harness::sandbox::Enforcement::Failed {
                reason: err.to_string(),
            },
        },
    };

    match harness::sandbox::reentry_action(&decision, &enforcement) {
        harness::sandbox::ReentryAction::Exec => {
            exec_pending_command(&command, &cwd, &extra_readonly_paths)
        }
        harness::sandbox::ReentryAction::Refuse { reason, exit_code } => {
            // On Windows the policy judgement parked a prepared AppContainer setup behind the
            // `Ok(())` it just returned, and that setup carries live grants on the workspace
            // DACLs plus a registered AppContainer profile. Nothing is going to launch a
            // confined child from this process any more, so release them before exiting —
            // otherwise the refusal leaves the workspace ACL-extended and the profile behind
            // until process teardown.
            #[cfg(target_os = "windows")]
            {
                harness::sandbox_windows::discard_windows_policy_setup();
            }
            tracing::error!("refusing sandbox re-entry: {reason}");
            eprintln!("{}", harness::sandbox::refusal_message(&reason));
            std::process::exit(exit_code)
        }
    }
}

/// Canonicalize the working directory handed over by the spawn side. The enforcement
/// backend anchors its rules on resolved inodes (a Landlock ruleset on Linux, the rendered
/// Seatbelt profile on macOS) and the pending command inherits this directory, so a
/// relative or symlinked root (`.`, `~/wip`) is resolved once, here.
///
/// Fallback: if canonicalization fails (the directory vanished concurrently, a
/// component is unreadable), the raw path is kept and the failure goes to stderr — not
/// a fail-open: the sandbox resolves that path itself or the attempt fails, exiting
/// [`harness::sandbox::SANDBOX_REFUSE_EXIT_CODE`] without exec-ing anything.
fn canonical_sandbox_cwd(raw: &str) -> std::path::PathBuf {
    let path = std::path::Path::new(raw);
    match path.canonicalize() {
        Ok(canonical) => canonical,
        Err(err) => {
            eprintln!("warning: could not resolve sandbox working directory {raw}: {err}");
            path.to_path_buf()
        }
    }
}

/// Resolve a program token the way a shell would: a token that already contains `/` is
/// taken as given, otherwise each `PATH` entry is tried in order and the first candidate
/// found on disk wins. The candidate is then canonicalized — the resolve-and-keep-the-raw
/// idiom of [`canonical_sandbox_cwd`] — because a sandbox matches the **resolved** target
/// (the real image behind a symlink farm such as `/bin`), and that resolved path is the
/// only one a user can act on.
///
/// `None` means the token is nowhere on disk (no `PATH`, or no `PATH` entry holds it): a
/// not-found failure, which must never be reported as a sandbox denial.
#[cfg(unix)]
fn resolve_program_path(token: &str) -> Option<std::path::PathBuf> {
    let candidate = if token.contains('/') {
        Some(std::path::PathBuf::from(token))
    } else {
        std::env::var_os("PATH").and_then(|entries| {
            std::env::split_paths(&entries)
                .filter(|dir| !dir.as_os_str().is_empty())
                .map(|dir| dir.join(token))
                .find(|candidate| candidate.is_file())
        })
    }?;
    match candidate.canonicalize() {
        Ok(canonical) => Some(canonical),
        // Canonicalization failed (unreadable component, or a sandbox that refuses the
        // symlink read): the unresolved candidate is still the honest answer.
        Err(_) => Some(candidate),
    }
}

/// Launch the pending command in `cwd`, under the sandbox this process has established, and
/// terminate the re-entry child with the command's exit code. Reached only for a
/// [`harness::sandbox::ReentryAction::Exec`]. The mechanism is per platform, and the
/// confinement is the point of each form:
///
/// * **unix** — `execve()` of `sh -c <command>`, so the pending command *becomes* this
///   already-restricted process, which is why a refusal to exec is a sandbox statement;
/// * **Windows** — no `execve()`, and no `sh`: the pending command is launched as a child
///   **inside the AppContainer this process prepared**, through the launch half of the Windows
///   backend, which releases the policy guards once that child has exited. The setup prepared
///   by the policy pass above is picked up by that call rather than prepared a second time;
/// * **any other target** — the spawn-and-wait fallback this function used before the re-entry
///   became platform-specific.
///
/// `extra_readonly_paths` are the configured extras exactly as they arrived through the
/// environment. Only the Windows launch consumes them (a setup re-prepared inside that call
/// repeats the same policy); on unix they were already folded into the ruleset by the
/// enforcement pass, so this arm says what it is ignoring.
fn exec_pending_command(
    command: &str,
    cwd: &std::path::Path,
    extra_readonly_paths: &[String],
) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // The extras list is consumed by the enforcement pass above, not by this step: it never
        // reaches `execve()`, which takes a program, argv and environment only.
        tracing::debug!(
            extra_readonly_paths = extra_readonly_paths.len(),
            "sandbox re-entry: replacing this process with the pending command"
        );
        // `exec()` only returns when it failed, i.e. the process is still unsandboxed
        // or about to be — so it terminates the process instead of continuing.
        let err = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .exec();
        // The bare token is unactionable: on a package-store host (`/bin` as a symlink
        // farm) the exec that was refused is the **resolved** image, not `sh`. Name the
        // sandbox and that resolved path; fall back to the token when it resolves to
        // nothing, and keep the cause honest — only a refusal is an allow-list gap.
        let program = resolve_program_path("sh")
            .map(|resolved| resolved.display().to_string())
            .unwrap_or_else(|| "sh".to_string());
        let cause = if err.kind() == std::io::ErrorKind::PermissionDenied {
            " The path is not covered by the sandbox allow-list."
        } else {
            " Check that the program exists and is executable."
        };
        eprintln!("error: failed to exec {program} under the active sandbox: {err}.{cause}");
        std::process::exit(1)
    }
    #[cfg(target_os = "windows")]
    {
        // The confined launch. The AppContainer setup parked behind the `Ok(())` of the
        // enforcement call above is picked up by this call, the command is launched inside it,
        // and the guards are released once the child has exited. `cwd` is passed as the policy
        // root because that is the root the setup was prepared for: the spawn side handed over
        // the workspace, and the policy pass anchored on the same canonicalised directory.
        match harness::sandbox_windows::run_in_appcontainer(cwd, extra_readonly_paths, cwd, command)
        {
            // The child's own exit code is this run's verdict, so it is the exit code of the
            // re-entry child too: a confined command that exits 3 does not report 0.
            Ok(code) => std::process::exit(code),
            Err(err) => {
                // The confinement itself failed, so the command never ran confined. Report it in
                // the same register as the unix exec failure — the sandbox is named, and the
                // reason is the backend's own — and exit with the refusal code rather than
                // running the command in the clear or reporting success.
                tracing::error!(
                    kind = harness::sandbox::SandboxError::kind(&err),
                    "sandbox re-entry: the confined launch failed: {err}"
                );
                eprintln!("error: failed to run the command under the active sandbox: {err}");
                std::process::exit(harness::sandbox::SANDBOX_REFUSE_EXIT_CODE);
            }
        }
    }
    #[cfg(not(any(unix, target_os = "windows")))]
    {
        let _ = extra_readonly_paths;
        let mut child = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .spawn()?;
        let status = child.wait()?;
        std::process::exit(status.code().unwrap_or(1))
    }
}

fn boot_manager(cfg: &config::Config) -> std::sync::Arc<orchestrator::OrchestratorManager> {
    let plan = manager::phase::Plan::default();
    let stats = std::sync::Arc::new(harness::HarnessStats::new());
    let client = llm::ChatClient::from_config(cfg);
    std::sync::Arc::new(orchestrator::OrchestratorManager::from_config(
        client, plan, stats, cfg,
    ))
}

fn parse_args() -> CliArgs {
    let mut args = CliArgs::default();
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--config" => {
                if let Some(v) = it.next() {
                    args.config = Some(v);
                } else {
                    eprintln!("error: --config requires a path");
                    std::process::exit(2);
                }
            }
            "--raw" => args.raw = true,
            "--debug" => args.debug = true,
            // Both arms MUST sit above the catch-all `other =>` below: that arm
            // turns an unrecognized argument into the positional PROMPT, so a
            // forgotten arm would make `--resume` the session's goal text.
            "--resume" => args.resume = true,
            "--fresh" => args.fresh = true,
            "--keep-session" => args.keep_session = true,
            "-h" | "--help" => {
                println!(
                    "marmel — autonomous agentic coding assistant\n\n\
                     USAGE:\n    marmel [--config <path>] [--raw] [--debug] [--keep-session] [--resume | --fresh] [PROMPT]\n\n\
                     FLAGS:\n    --raw            force headless stdout (pipe-friendly) mode\n\
                     --debug          log all incoming and outgoing LLM and tool traffic to debug.log\n\
                     --config <path>  override the config file path\n\
                     --keep-session   preserve session and runtime files in .marmel/ after completion\n\
                     --resume         rehydrate the previous run from .marmel/session/\n\
                     --fresh          start fresh even if session_resume / MARMEL_RESUME is set or the plan is unfinished\n\
                     -h, --help       print this help\n\n\
                     ARGS:\n    PROMPT           optional initial prompt to start the session"
                );
                std::process::exit(0);
            }
            other => {
                if args.prompt.is_none() {
                    args.prompt = Some(other.to_string());
                } else {
                    eprintln!("ignoring extra argument: {other}");
                }
            }
        }
    }
    args
}

const DEFAULT_MAX_LOG_BYTES: u64 = 5 * 1024 * 1024;
const LOG_BACKUPS: u32 = 3;

/// The session-flag usage errors, resolved once in `main` after `config::load` and
/// reported like the valueless `--config` precedent (`eprintln!` + `exit(2)`):
/// `--resume` **and** `--fresh` are contradictory, and `--resume` while capture is
/// disabled asks this run to read a bundle it would never have written
/// (`cfg.session_enabled` folds in `MARMEL_SESSION_DISABLE`, see
/// [`config::apply_session_env`]). `--fresh` alone is never an error: it is the
/// documented escape hatch, overriding `session_resume = true` / `MARMEL_RESUME=1`.
fn session_flag_error(args: &CliArgs, cfg: &config::Config) -> Option<&'static str> {
    if args.resume && args.fresh {
        return Some("--resume and --fresh are mutually exclusive");
    }
    if args.resume && !cfg.session_enabled {
        return Some(
            "--resume was requested but session capture is disabled (session_enabled = false \
             or MARMEL_SESSION_DISABLE=1): this run would never have written the bundle it is \
             being asked to read",
        );
    }
    None
}

fn rotate_log(path: &std::path::Path, max_bytes: u64, backups: u32) {
    harness::workspace::rotate_log_file(path, max_bytes, backups);
}

/// Report the shared HTTP client's construction verdict exactly once, at startup.
///
/// On a host with no TLS trust store the client cannot be built at all: every
/// LLM turn of this run then fails the same way, because each one is refused with
/// the non-retryable `ChatError::TrustStore` before a request is ever sent. The
/// run is not aborted for it (no new exit code, and no panic as happened when the
/// client build was `unwrap_or_default()`d): it gets one actionable line naming
/// the missing CA bundles and how to point marmel at them — the
/// `ca-certificates` package, `SSL_CERT_FILE`, `SSL_CERT_DIR`.
///
/// `tracing` is already configured by [`setup_panic_hook`]: in raw mode its sink
/// **is** stderr, so the single record is the user-facing line; in TUI mode it
/// goes to `.marmel/marmel.log`, where stderr is the only channel the user can
/// still see before the alternate screen takes over.
fn report_http_client_verdict(use_raw: bool) {
    let Some(message) = llm::http_client_startup_error() else {
        return;
    };
    tracing::error!("{message}");
    if !use_raw {
        eprintln!("error: {message}");
    }
}

fn setup_panic_hook(use_raw: bool) {
    if use_raw {
        let _ = tracing_subscriber::fmt()
            .with_writer(std::io::stderr)
            .with_env_filter(
                tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                    tracing_subscriber::EnvFilter::new("info,tui_markdown=error")
                }),
            )
            .try_init();
    } else {
        let workspace = harness::workspace::Workspace::new();
        let log_path = workspace
            .as_ref()
            .map(|ws| ws.log_path())
            .unwrap_or_else(|_| std::path::PathBuf::from(".marmel/marmel.log"));
        rotate_log(&log_path, DEFAULT_MAX_LOG_BYTES, LOG_BACKUPS);
        if let Ok(file) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)
        {
            let _ = tracing_subscriber::fmt()
                .with_writer(file)
                .with_ansi(false)
                .with_env_filter(
                    tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
                        tracing_subscriber::EnvFilter::new("info,tui_markdown=error")
                    }),
                )
                .try_init();
        }
    }

    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ui::restore();
        }));
        // Flush the session bundle before the panic message is printed, so a crashed
        // run leaves behind a bundle that says `panicHook` rather than `inProgress`.
        // `commit_from_panic` keeps what this hook depends on: pure synchronous I/O (no
        // runtime is built while one is unwinding), lock-poison tolerance (every guard
        // takes `into_inner()` on a poisoned lock), and no blocking — the session's
        // `CommitLock` is only *tried*, so a panic inside a commit degrades honestly
        // (the bundle keeps `inProgress`, the next boot infers `HardKillInferred`).
        // `ui::restore()` above still runs first, and this flush is wrapped too.
        let flushed = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            marmennill::ui::session::commit_from_panic()
        }));
        match flushed {
            Ok(outcome) => eprintln!("session flush: {}", outcome.detail),
            Err(_) => eprintln!(
                "session flush: the panic-hook flush itself unwound, so {} was left exactly as \
                 it was last committed (its manifest keeps the ending it already carried)",
                marmennill::ui::session::panic_target_bundle_hint()
            ),
        }
        default_hook(info);
    }));
}

fn stdout_is_terminal() -> bool {
    use std::io::IsTerminal;
    std::io::stdout().is_terminal()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_rotate_log_over_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let log = dir.join("marmel.log");
        fs::write(&log, "x".repeat(100)).unwrap();

        rotate_log(&log, 10, 3);

        assert!(
            !log.exists() || fs::metadata(&log).unwrap().len() == 0,
            "fresh log must be empty"
        );
        assert_eq!(
            fs::read_to_string(dir.join("marmel.log.1")).unwrap(),
            "x".repeat(100),
            "old contents moved to backup"
        );
    }

    #[test]
    fn test_rotate_log_under_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let log = dir.join("marmel.log");
        fs::write(&log, "small").unwrap();

        rotate_log(&log, 100, 3);

        assert_eq!(fs::read_to_string(&log).unwrap(), "small");
        assert!(!dir.join("marmel.log.1").exists());
    }

    #[test]
    fn test_rotate_log_shifts_backups() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let log = dir.join("marmel.log");
        fs::write(&log, "current").unwrap();
        fs::write(dir.join("marmel.log.1"), "one").unwrap();
        fs::write(dir.join("marmel.log.2"), "two").unwrap();
        fs::write(dir.join("marmel.log.3"), "three").unwrap();

        rotate_log(&log, 1, 3);

        assert_eq!(
            fs::read_to_string(dir.join("marmel.log.1")).unwrap(),
            "current"
        );
        assert_eq!(fs::read_to_string(dir.join("marmel.log.2")).unwrap(), "one");
        assert_eq!(fs::read_to_string(dir.join("marmel.log.3")).unwrap(), "two");
        assert!(!dir.join("marmel.log.4").exists());
    }

    #[test]
    fn test_rotate_log_missing_file() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path();
        let log = dir.join("does-not-exist.log");
        rotate_log(&log, 10, 3);
        assert!(!log.exists());
    }

    /// Both ends of the re-entry protocol must speak the same sentinel; a private copy
    /// of the literal in `main.rs` is what lets the two ends drift apart.
    #[test]
    fn test_sandbox_reentry_recognizes_the_shared_sentinel_constant() {
        assert!(is_sandbox_reentry(Some(harness::pty::SANDBOX_EXEC_ARG)));
        assert!(!is_sandbox_reentry(Some("--raw")));
        assert!(!is_sandbox_reentry(None));
    }

    /// A refusal must never look like a successful run.
    #[test]
    fn test_sandbox_refusal_exit_code_is_non_zero() {
        assert_ne!(harness::sandbox::SANDBOX_REFUSE_EXIT_CODE, 0);
    }

    /// The re-entry cwd is derived canonically, so a symlinked or relative root is
    /// resolved once instead of re-derived per side of the protocol.
    /// Both sides of the comparison are canonicalized so the assertion is platform
    /// neutral: on macOS `tempdir()` sits under `/var/folders/...` and `canonicalize`
    /// resolves the `/var` symlink to `/private/var`.
    #[cfg(unix)]
    #[test]
    fn test_canonical_sandbox_cwd_resolves_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let resolved = canonical_sandbox_cwd(&link.to_string_lossy());
        // The expectation has to go through the same canonicalization, otherwise the
        // comparison only passes on platforms without symlinked temp roots.
        let expected = std::fs::canonicalize(&real).unwrap();

        assert_eq!(resolved, expected);
        // Not vacuous: the returned path must be the resolved target, never the symlink.
        assert_ne!(resolved, link);
    }

    /// Documented fallback: an unresolvable path is kept as given (and reported), and
    /// the sandbox attempt then decides — failure is fail-closed, not a widening.
    #[test]
    fn test_canonical_sandbox_cwd_falls_back_to_the_raw_path() {
        let raw = "/definitely/not/a/directory/marmel-t052";
        assert_eq!(canonical_sandbox_cwd(raw), std::path::PathBuf::from(raw));
    }

    /// The re-entry exec failure prints the **resolved** program, so the lookup is
    /// pinned directly: a bare token goes through `PATH` and comes back canonical.
    #[cfg(unix)]
    #[test]
    fn test_resolve_program_path_resolves_a_bare_token_through_path() {
        let resolved = resolve_program_path("sh").expect("`sh` must resolve on a unix host");
        assert!(resolved.is_absolute(), "{resolved:?} must be absolute");
        assert!(resolved.is_file(), "{resolved:?} must be a file");
        // Already canonical: re-resolving is a no-op, so what gets printed is exactly
        // the image the exec (and the sandbox check) was about.
        assert_eq!(resolved, std::fs::canonicalize(&resolved).unwrap());
    }

    /// A token that already contains `/` skips the `PATH` walk and is resolved to the
    /// image behind it — the `/bin/sh -> /nix/store/…-bash/bin/sh` case from the report.
    #[cfg(unix)]
    #[test]
    fn test_resolve_program_path_resolves_an_absolute_symlinked_token() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real-sh");
        fs::write(&real, "#!/bin/sh\n").unwrap();
        let link = temp.path().join("link-sh");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        let resolved = resolve_program_path(&link.to_string_lossy())
            .expect("an absolute token always produces a path");
        assert_eq!(resolved, std::fs::canonicalize(&real).unwrap());
        // Not vacuous: the symlink itself is never what gets reported.
        assert_ne!(resolved, link);
    }

    /// Both documented fallbacks, so the message can never invent a path: an absolute
    /// token that is not on disk is kept as given, and a bare token with no `PATH` hit
    /// is reported as not-found (never as a sandbox denial).
    #[cfg(unix)]
    #[test]
    fn test_resolve_program_path_falls_back_without_inventing_a_path() {
        let raw = "/definitely/not/a/program/marmel-t052-sh";
        assert_eq!(
            resolve_program_path(raw),
            Some(std::path::PathBuf::from(raw))
        );
        assert_eq!(resolve_program_path("marmel-no-such-program-t052"), None);
    }

    // ------------------------------------------------------------- session flags

    #[test]
    fn session_flags_default_to_absent() {
        let args = CliArgs::default();
        assert!(!args.resume);
        assert!(!args.fresh);
        assert!(!args.keep_session);
    }

    /// No flags at all must produce no error, whatever the config says — the run
    /// behaves exactly as an ordinary start.
    #[test]
    fn absent_session_flags_are_never_an_error() {
        let args = CliArgs {
            prompt: Some("do the thing".to_string()),
            ..CliArgs::default()
        };
        let cfg = config::Config::default();
        assert!(session_flag_error(&args, &cfg).is_none());

        let cfg = config::Config {
            session_enabled: false,
            session_resume: true,
            ..config::Config::default()
        };
        assert!(
            session_flag_error(&args, &cfg).is_none(),
            "--fresh/--resume absent: config alone is not a usage error"
        );
    }

    #[test]
    fn resume_and_fresh_together_is_a_usage_error() {
        let args = CliArgs {
            resume: true,
            fresh: true,
            ..CliArgs::default()
        };
        let error = session_flag_error(&args, &config::Config::default())
            .expect("--resume --fresh must be refused");
        assert!(
            error.contains("--resume") && error.contains("--fresh"),
            "{error}"
        );
    }

    #[test]
    fn resume_with_capture_disabled_is_a_usage_error() {
        let args = CliArgs {
            resume: true,
            ..CliArgs::default()
        };
        let cfg = config::Config::default();
        assert!(session_flag_error(&args, &cfg).is_none());
        let cfg = config::Config {
            session_enabled: false,
            ..config::Config::default()
        };
        let error =
            session_flag_error(&args, &cfg).expect("--resume with capture off must be refused");
        assert!(error.contains("session_enabled"), "{error}");
    }

    #[test]
    fn fresh_alone_is_the_escape_hatch() {
        let args = CliArgs {
            fresh: true,
            ..CliArgs::default()
        };
        let cfg = config::Config {
            session_enabled: false,
            session_resume: true,
            ..config::Config::default()
        };
        assert!(session_flag_error(&args, &cfg).is_none());
    }

    /// Placement is load-bearing: the catch-all `other =>` arm turns an
    /// unrecognized argument into the positional PROMPT, so both new arms must
    /// appear **before** it in `parse_args`.
    #[test]
    fn session_flag_arms_precede_the_prompt_catch_all() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs must be readable");
        let parser = src
            .find("fn parse_args()")
            .map(|at| src[at..].to_string())
            .expect("parse_args must exist");
        for (flag, field) in [
            ("\"--resume\"", "resume"),
            ("\"--fresh\"", "fresh"),
            ("\"--keep-session\"", "keep_session"),
        ] {
            let arm = parser
                .find(flag)
                .unwrap_or_else(|| panic!("parse_args must have a {flag} arm"));
            let catch_all = parser
                .find("other => {")
                .expect("parse_args must keep the PROMPT catch-all arm");
            assert!(arm < catch_all, "{flag} would become the PROMPT");
            assert!(
                parser.contains(&format!("{flag} => args.{field}")),
                "the {flag} arm must set CliArgs::{field}"
            );
        }
        assert!(
            parser.contains("--resume | --fresh") || parser.contains("--resume         rehydrate"),
            "the help block must document both flags"
        );
    }

    // ---------------------------------------------------------------- signals (#t-015)

    /// Shared by the scans below: `main()` is one function, so its text can be sliced out
    /// the same way the MCP boot test does it.
    fn main_fn_text(src: &str) -> String {
        let from = src
            .find("fn main() -> Result<()>")
            .expect("main() must stay one function");
        let rest = &src[from..];
        let end = rest
            .find("fn is_sandbox_reentry(")
            .expect("`fn is_sandbox_reentry(` must follow main()");
        rest[..end].to_string()
    }

    /// The signal wiring is the difference between a bounded reap and an orphan: the
    /// watcher must be installed **before** the boot task can spawn a child, the run must
    /// be composed with the signal future on the runtime, and the outcome of that race
    /// must hand over to the very same teardown lines the normal exit path runs.
    #[test]
    fn exit_signal_wiring_is_installed_before_mcp_boot_and_reaches_the_same_teardown() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs must be readable");
        let main_fn = main_fn_text(&src);

        // (a) Handlers live before the first stdio child can exist.
        let install = main_fn
            .find("install_exit_signal_watcher(&rt)")
            .expect("main must install the SIGINT/SIGTERM watcher");
        let boot = main_fn
            .find("mcp::McpManager::spawn_boot(rt.handle(), &cfg.mcp_servers)")
            .expect("MCP boot must stay in main()");
        assert!(
            install < boot,
            "the exit-signal watcher must be installed before `spawn_boot`, or a signal \
             during MCP startup orphans the still-booting child"
        );

        // (b) Both run modes are raced against the signal future, not awaited bare.
        assert_eq!(
            main_fn
                .matches("rt.block_on(run_reacting_to_exit_signal(")
                .count(),
            2,
            "both the raw and the TUI run must be composed with the exit-signal future"
        );
        assert!(
            main_fn.contains("SIGNAL_RUN_GRACE,"),
            "the grace handed to the race must be the documented, bounded one"
        );

        // (c) The signal outcome falls through to the SAME teardown: watcher stopped,
        // then cancel_all, then the MCP reap, then the runtime stop.
        let stop_watch = main_fn
            .find("signal.stop()")
            .expect("the watcher must be stopped once the run has decided how it ends");
        let cancel = main_fn
            .find("orchestrator::cancel_all();")
            .expect("every worker must be cancelled before teardown");
        let reap = main_fn
            .find("shutdown_boot_task()")
            .expect("the boot task must be aborted on the exit path");
        let stop = main_fn
            .find("rt.shutdown_timeout(")
            .expect("the runtime must be shut down with a timeout");
        let verdict = main_fn
            .find("RunOutcome::Signalled =>")
            .expect("a signal-stopped run must report a verdict");
        assert!(
            stop_watch < cancel && cancel < reap && reap < stop && stop < verdict,
            "the watcher must be stopped before the teardown runs, the teardown must stay \
             between cancel_all() and shutdown_timeout(), and the signal verdict must be \
             taken only AFTER the bounded MCP teardown"
        );
        assert!(
            main_fn.contains("std::process::exit(arrival.exit_code())"),
            "a run stopped by a signal must exit with the signal's own exit code"
        );
        assert!(
            main_fn.contains("RunOutcome::Completed(result) => result"),
            "the run's own verdict must still win when the run finishes on its own"
        );

        // (d) A run abandoned mid-TUI must not leave the terminal in raw mode / the
        // alternate screen behind.
        assert!(
            main_fn.contains("RunOutcome::Signalled") && main_fn.contains("ui::restore();"),
            "the signal path must restore the terminal before the teardown"
        );
    }

    /// The raw/TUI caveat is a documented decision, pinned here: Ctrl-C while the terminal
    /// is in raw mode stays a crossterm key event owned by the TUI's own quit path
    /// (`request_user_exit`), and a real signal arriving during the run deliberately
    /// replays that same quit path before it forces the bounded teardown.
    #[test]
    fn raw_mode_ctrl_c_stays_with_the_ui_quit_path_and_a_real_signal_replays_it() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs must be readable");
        let main_fn = main_fn_text(&src);
        let caveat = main_fn
            .find("let signal = install_exit_signal_watcher(&rt);")
            .expect("the watcher must be installed by main()");
        let comment = &main_fn[..caveat];
        assert!(
            comment.contains("Ctrl-C") && comment.contains("crossterm"),
            "main() must document that Ctrl-C in raw mode is a crossterm key event, not a \
             signal, so the TUI's handling is untouched"
        );
        assert!(
            comment.contains("request_user_exit"),
            "main() must name the UI's own quit path it defers to"
        );
        assert!(
            comment.contains("orchestrator::cancel_all()"),
            "main() must document that a real signal replays the UI quit path's cancel_all \
             before forcing the bounded teardown"
        );
    }

    /// The race must stay bounded, and it must give the UI's own quit path the first turn.
    #[test]
    fn the_signal_race_is_bounded_and_lets_the_run_quit_itself_first() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs must be readable");
        let from = src
            .find("async fn run_reacting_to_exit_signal")
            .expect("the signal race must exist");
        let rest = &src[from..];
        let end = rest
            .find("\nfn main() -> Result<()>")
            .expect("the race is defined immediately before main()");
        let race = &rest[..end];

        // The grace reuses the existing 250 ms budget: no new, invented wait.
        assert!(
            src.contains("const SIGNAL_RUN_GRACE: std::time::Duration = std::time::Duration::from_millis(250);"),
            "the run grace must be the 250 ms budget the MCP sweep already uses"
        );
        // Nothing unbounded is awaited on the signal path: the wait for the run is a
        // `timeout`, there is no `pending()`/`sleep`-forever stand-in, and no new blocking
        // wait is introduced.
        assert!(
            race.contains("tokio::time::timeout(grace, &mut run)")
                && race.contains("_ = state.force_request.notified()"),
            "the grace must be a timeout, and a second arrival must break it"
        );
        assert!(
            !race.contains("block_on") && !race.contains("std::future::pending"),
            "the signal path must not add a blocking or open-ended wait"
        );
        // The UI's own quit path runs first: `cancel_all()` (exactly what
        // `request_user_exit` does) precedes the grace, and the skip-the-run branch comes
        // before everything else.
        let skip = race
            .find("state.arrival_count() > 0")
            .expect("a signal that arrived before the run must skip the run");
        let cancel = race
            .find("orchestrator::cancel_all();")
            .expect("the signal path must cancel every worker");
        let timed = race
            .find("tokio::time::timeout(grace,")
            .expect("the grace must be a timeout");
        assert!(
            skip < cancel && cancel < timed,
            "a signal before the run skips it; during the run the UI's own quit path is \
             triggered before the bounded grace"
        );
    }

    /// The counting rules, driven for real: one arrival requests the exit, every later one
    // forces it, and the permits are exact — no storm of wakeups, no lost arrival.
    #[test]
    fn the_first_arrival_requests_the_exit_and_later_arrivals_force_it() {
        let state = SignalState::default();
        assert_eq!(state.arrival_count(), 0);
        assert_eq!(state.observe(SignalArrival::Terminate), 1);
        assert_eq!(state.observe(SignalArrival::Interrupt), 2);
        assert_eq!(state.observe(SignalArrival::Interrupt), 3);
        assert_eq!(state.arrival_count(), 3);
        assert_eq!(
            state.exit_arrival(),
            SignalArrival::Terminate,
            "the exit verdict names the arrival that requested it, not a later one"
        );

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime to drive the notifies");
        let state = &state;
        rt.block_on(async move {
            let bound = std::time::Duration::from_millis(200);
            // The exit request fires exactly once …
            assert!(
                tokio::time::timeout(bound, state.exit_request.notified())
                    .await
                    .is_ok(),
                "the first arrival must have requested the exit"
            );
            assert!(
                tokio::time::timeout(bound, state.exit_request.notified())
                    .await
                    .is_err(),
                "only the first arrival may request the exit: no permit may be left over"
            );
            // … and later arrivals merge into exactly **one** force wake-up: a `Notify`
            // stores at most one permit, which is precisely what the bounded path needs —
            // it consumes that permit once (or not at all) and never waits again.
            assert!(
                tokio::time::timeout(bound, state.force_request.notified())
                    .await
                    .is_ok(),
                "a later arrival must force the teardown"
            );
            assert!(
                tokio::time::timeout(bound, state.force_request.notified())
                    .await
                    .is_err(),
                "the force permits merge: arrivals 2 and 3 leave exactly one wake-up, so a \
                 repeated press cannot build up a queue of wake-ups"
            );
        });
    }

    /// The exit codes and names this wiring reports, pinned: the shell convention
    /// `128 + signum`, spelled the way `session::format::ProcessEnding::Signal` spells it.
    #[test]
    fn signal_exit_codes_follow_the_shell_convention_and_names_are_spelled_not_numbers() {
        assert_eq!(SignalArrival::Interrupt.name(), "SIGINT");
        assert_eq!(SignalArrival::Terminate.name(), "SIGTERM");
        assert_eq!(SignalArrival::Interrupt.signum(), 2);
        assert_eq!(SignalArrival::Terminate.signum(), 15);
        assert_eq!(SignalArrival::Interrupt.exit_code(), 130);
        assert_eq!(SignalArrival::Terminate.exit_code(), 143);
        // The shared marker round-trips, and `0` stays the "no arrival yet" sentinel.
        for arrival in [SignalArrival::Interrupt, SignalArrival::Terminate] {
            assert_eq!(SignalArrival::from_marker(arrival.marker()), Some(arrival));
        }
        assert_eq!(SignalArrival::from_marker(0), None);
        assert_eq!(SignalArrival::from_marker(9), None);
    }

    /// Behavioural proof, not a text scan: a run that never finishes must be stopped by the
    /// first arrival, inside its grace, and the signal path must have cancelled the
    /// in-flight work so the session loop's own abort/commit path got its turn.
    #[test]
    fn a_signal_arriving_mid_run_stops_the_run_inside_the_grace() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the race");
        let state = std::sync::Arc::new(SignalState::default());
        let began = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let grace = std::time::Duration::from_millis(60);
        let observed = std::sync::Arc::clone(&state);
        let began_run = std::sync::Arc::clone(&began);

        rt.block_on(async move {
            let signaller = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                observed.observe(SignalArrival::Terminate);
            });
            let started = std::time::Instant::now();
            let outcome = run_reacting_to_exit_signal(state.as_ref(), grace, async move {
                began_run.store(true, std::sync::atomic::Ordering::Relaxed);
                std::future::pending::<()>().await;
                Ok(())
            })
            .await;
            let elapsed = started.elapsed();
            signaller.await.unwrap();

            assert!(
                matches!(outcome, RunOutcome::Signalled),
                "the never-ending run must have been abandoned, got {outcome:?}"
            );
            assert!(
                began.load(std::sync::atomic::Ordering::Relaxed),
                "the run must have started before the signal stopped it"
            );
            assert!(
                elapsed >= grace && elapsed < grace + std::time::Duration::from_millis(500),
                "the grace is the bound: measured {elapsed:?} against grace {grace:?}"
            );
            assert!(
                orchestrator::is_globally_cancelled(),
                "the signal path must first replay the UI quit path's `cancel_all()`, so the \
                 session loop's own abort/commit path gets its turn before the run is \
                 abandoned"
            );
            // Leave the process-global token as we found it: this test cancels, so the
            // reset is part of the assertion, not an afterthought.
            orchestrator::reset_cancellation();
            assert_eq!(
                state.exit_arrival().exit_code(),
                143,
                "the arrival that stopped the run is the one the exit code names"
            );
        });
    }

    /// The twice-over case must not deadlock: a second arrival breaks the grace
    /// immediately instead of waiting it out.
    #[test]
    fn a_second_arrival_skips_the_grace_instead_of_deadlocking() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the race");
        let state = std::sync::Arc::new(SignalState::default());
        let began = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let grace = std::time::Duration::from_millis(2_000);
        let observed = std::sync::Arc::clone(&state);
        let began_run = std::sync::Arc::clone(&began);

        rt.block_on(async move {
            let signaller = tokio::spawn(async move {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                observed.observe(SignalArrival::Interrupt);
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                observed.observe(SignalArrival::Interrupt);
            });
            let started = std::time::Instant::now();
            let outcome = run_reacting_to_exit_signal(state.as_ref(), grace, async move {
                began_run.store(true, std::sync::atomic::Ordering::Relaxed);
                std::future::pending::<()>().await;
                Ok(())
            })
            .await;
            let elapsed = started.elapsed();
            signaller.await.unwrap();

            assert!(
                matches!(outcome, RunOutcome::Signalled),
                "the forced arrival must have ended the run"
            );
            assert!(
                elapsed < std::time::Duration::from_millis(1_000),
                "the grace must NOT have been waited out: measured {elapsed:?} against \
                 grace {grace:?}"
            );
        });
    }

    /// A signal that lands while MCP is still booting means "do not start this run at
    /// all": the run is skipped instead of being started and then abandoned.
    #[test]
    fn a_signal_before_the_run_starts_never_starts_the_run() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the race");
        let state = std::sync::Arc::new(SignalState::default());
        // The watcher records the arrival while MCP boots, before any run exists.
        assert_eq!(state.observe(SignalArrival::Interrupt), 1);
        let began = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let began_run = std::sync::Arc::clone(&began);

        let outcome = rt.block_on(run_reacting_to_exit_signal(
            state.as_ref(),
            std::time::Duration::from_millis(500),
            async move {
                began_run.store(true, std::sync::atomic::Ordering::Relaxed);
                std::future::pending::<()>().await;
                Ok(())
            },
        ));

        assert!(
            matches!(outcome, RunOutcome::Signalled),
            "the run must be reported as signal-stopped, got {outcome:?}"
        );
        assert!(
            !began.load(std::sync::atomic::Ordering::Relaxed),
            "a run that never started must not be started by the signal path"
        );
        assert_eq!(state.exit_arrival().exit_code(), 130);
    }

    /// The normal exit path is unchanged: a run that ends on its own keeps its own verdict,
    /// which is what preserves the `cleanTail` commit-on-exit behaviour.
    #[test]
    fn a_run_that_ends_on_its_own_keeps_its_verdict_and_commits() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("a runtime for the race");
        let state = std::sync::Arc::new(SignalState::default());

        let outcome = rt.block_on(run_reacting_to_exit_signal(
            state.as_ref(),
            std::time::Duration::from_millis(500),
            async { Ok(()) },
        ));

        assert!(
            matches!(outcome, RunOutcome::Completed(Ok(()))),
            "the run's own verdict must survive, got {outcome:?}"
        );
        assert_eq!(state.arrival_count(), 0, "no signal, no signal verdict");
    }

    /// `setup_panic_hook` must keep working next to the new wiring: a panic still restores
    /// the terminal, still flushes the bundle, and the signal watcher is stopped before the
    /// runtime is shut down so it can never turn a panic into a hang.
    #[test]
    fn the_panic_hook_still_flushes_and_the_watcher_stops_before_runtime_shutdown() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs must be readable");
        let from = src
            .find("fn setup_panic_hook(use_raw: bool)")
            .expect("setup_panic_hook must exist");
        let rest = &src[from..];
        let end = rest
            .find("\nfn stdout_is_terminal()")
            .expect("setup_panic_hook is followed by stdout_is_terminal");
        let hook = &rest[..end];
        assert!(
            hook.contains("std::panic::set_hook")
                && hook.contains("ui::restore()")
                && hook.contains("commit_from_panic()"),
            "the panic hook must restore the terminal and flush the session bundle"
        );
        assert!(
            hook.contains("catch_unwind"),
            "the hook must stay panic-safe, so a panic cannot hang the process"
        );

        let main_fn = main_fn_text(&src);
        let stop_watch = main_fn
            .find("signal.stop()")
            .expect("the watcher must be stopped first");
        let shutdown = main_fn
            .find("rt.shutdown_timeout(")
            .expect("the runtime must be shut down with a timeout");
        assert!(
            stop_watch < shutdown,
            "the watcher must be aborted before the runtime is shut down: nothing may be \
             left awaiting a signal while the process unwinds"
        );
    }

    /// Fast start is a contract, not an optimisation: `main` must reach the UI **without
    /// ever awaiting the MCP boot**, must keep the gate slot's writes inside `main`
    /// (exactly two of them: the immediate registration of the still-filling manager, and
    /// the re-registration once the verdict is `Ready`), and must reap MCP children on the
    /// exit path so no stdio server is orphaned. The only executable proof of that behaviour
    /// is a real stdio MCP server, which nothing on this host can spawn — so, like the
    /// sandbox re-entry tests above, it is pinned at the source text.
    #[test]
    fn mcp_boot_is_async_and_main_owns_every_gate_write() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/main.rs"))
            .expect("src/main.rs must be readable");
        // Scope the scan to `main()` itself: the assertions below spell out the very
        // strings they look for, so a scan of the whole file would match the test too.
        let main_fn = src
            .find("fn main() -> Result<()>")
            .map(|from| {
                let rest = &src[from..];
                let end = rest
                    .find("fn is_sandbox_reentry(")
                    .expect("`fn is_sandbox_reentry(` must follow main()");
                rest[..end].to_string()
            })
            .expect("main() must stay one function");

        // (a) Nothing on the way into the UI may block on MCP.
        assert!(
            main_fn.contains("mcp::McpManager::spawn_boot(rt.handle(), &cfg.mcp_servers)"),
            "MCP must be started with `spawn_boot` on the runtime main is running on"
        );
        assert!(
            !main_fn.contains("rt.block_on(mcp")
                && !main_fn.contains("mcp::McpManager::boot(&cfg.mcp_servers)"),
            "the eager `rt.block_on(McpManager::boot…)` startup must not come back"
        );

        // (b) The readiness verdict is resolved by a continuation task, in this file:
        // re-registration on `Ready`, warn-and-retain on `Failed`.
        assert!(
            main_fn.contains(".await_ready().await"),
            "the continuation must await `await_ready`"
        );
        assert!(
            main_fn.contains("mcp::McpBootStatus::Ready =>")
                && main_fn.contains("mcp::McpBootStatus::Failed(reason)"),
            "the continuation must branch on the boot verdict"
        );
        let writers = main_fn.matches("harness::set_mcp_manager(").count();
        assert_eq!(
            writers, 2,
            "exactly two gate writes belong in main(): the immediate registration of the \
             still-filling manager and the re-registration once the boot is Ready"
        );

        // (c) The exit path reaps MCP children between cancelling every worker and
        // stopping the runtime, bounded so a wedged server can never stall the exit.
        let cancel = main_fn
            .find("orchestrator::cancel_all();")
            .expect("every worker must be cancelled before teardown");
        let stop = main_fn
            .find("rt.shutdown_timeout(")
            .expect("the runtime must be shut down with a timeout");
        let reap = main_fn
            .find("shutdown_boot_task()")
            .expect("the boot task must be aborted on the exit path");
        assert!(
            cancel < reap && reap < stop,
            "the MCP teardown must sit between cancel_all() and shutdown_timeout()"
        );
        assert!(
            main_fn.contains("tokio::time::timeout(std::time::Duration::from_millis(250)"),
            "the MCP client shutdown must be bounded"
        );
    }
}
