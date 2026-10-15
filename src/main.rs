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
    /// (equivalent to `MARMEL_RESUME=1` or `session_resume = true`). The boot-time
    /// restore is wired (t-012/t-024): `ui::session::set_resume_request` records the
    /// decision and the session loop boots from the bundle, exiting with code 3
    /// (`session::EXIT_RESTORE_REFUSED`) when a bundle cannot be restored.
    resume: bool,
    /// Force a fresh start: overrides `session_resume = true` and
    /// `MARMEL_RESUME=1`. `--fresh` always wins; it is the escape hatch.
    fresh: bool,
    prompt: Option<String>,
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
    // t-012 (flow §3.3): hand the CLI's resume decision to the module that owns
    // the session loop. `ui::tui::run` / `ui::raw::run` keep their signatures (they
    // belong to other tasks), so this one-shot handoff is the seam; the precedence
    // `--fresh` > `--resume` > `MARMEL_RESUME` / `session_resume` is documented at
    // `ui::session::resume_requested`, and the contradictory combinations were
    // rejected above (`exit 2`). Without either flag this records `false`/`false`
    // and the run behaves exactly as before.
    ui::session::set_resume_request(args.resume, args.fresh);

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

    if !cfg.mcp_servers.is_empty()
        && let Ok(mcp_mgr) = rt.block_on(mcp::McpManager::boot(&cfg.mcp_servers))
    {
        harness::set_mcp_manager(std::sync::Arc::new(mcp_mgr));
    }

    let manager = Some(boot_manager(&cfg));

    let res = if use_raw {
        rt.block_on(ui::raw::run(&cfg, args.prompt, manager))
    } else {
        rt.block_on(ui::tui::run(&cfg, args.prompt, manager))
    };

    orchestrator::cancel_all();
    rt.shutdown_timeout(std::time::Duration::from_millis(300));
    // t-012 (flow §3.5): a **refused** `--resume` is a first-class verdict, not a
    // runtime error. `main() -> Result<()>` can only ever exit `1`, so the typed
    // `session::RestoreRefused` the boot pass returned is mapped here — after the
    // teardown above has restored the screen and cancelled every worker — onto the
    // named constant `session::EXIT_RESTORE_REFUSED` (3). The refusal itself (code,
    // member, the byte_len/fingerprint pair, the bundle path, "the bundle was NOT
    // modified") was already printed through the session's renderer, journal and
    // log by the boot pass; it is repeated to stderr because the TUI status line is
    // gone by now.
    if let Err(err) = &res
        && err
            .downcast_ref::<marmennill::session::restore::RestoreRefused>()
            .is_some()
    {
        eprintln!("{err}");
        std::process::exit(marmennill::session::EXIT_RESTORE_REFUSED);
    }
    res
}

/// `true` when argv starts with the Landlock re-entry sentinel.
///
/// The sentinel comes from [`harness::pty::SANDBOX_EXEC_ARG`], the same constant the
/// spawn side uses, so the two ends of the protocol can never drift apart.
fn is_sandbox_reentry(first: Option<&str>) -> bool {
    first == Some(harness::pty::SANDBOX_EXEC_ARG)
}

/// The Landlock re-entry path: apply the sandbox to this process, then exec the
/// pending command — **only if enforcement was actually established**.
///
/// The order is load-bearing:
///
/// 1. resolve the working directory canonically ([`canonical_sandbox_cwd`]);
/// 2. honour the explicit opt-out ([`harness::pty::SANDBOX_OPTOUT_ENV`]) *before* any
///    Landlock call, and say so out loud ([`harness::sandbox::optout_warning`]);
/// 3. otherwise attempt enforcement and record what it achieved;
/// 4. ask [`harness::sandbox::reentry_action`] whether to exec. On
///    [`harness::sandbox::ReentryAction::Refuse`] the command is **never** run
///    unsandboxed: the reason goes to stderr and the process exits with
///    [`harness::sandbox::SANDBOX_REFUSE_EXIT_CODE`] (non-zero).
fn run_sandbox_reentry(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let cwd_raw = args.next().unwrap_or_else(|| ".".to_string());
    let command = args.next().unwrap_or_default();
    let cwd = canonical_sandbox_cwd(&cwd_raw);

    let resolved_exe = harness::pty::resolved_exe_path();
    let decision = harness::pty::should_apply_sandbox(&harness::pty::SandboxInputs {
        landlock_supported: cfg!(target_os = "linux"),
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
            tracing::warn!("Landlock sandbox disabled by {setting}: the command runs unsandboxed");
            harness::sandbox::Enforcement::Skipped {
                setting: setting.clone(),
            }
        }
        _ => match harness::sandbox::apply_sandbox(&cwd) {
            Ok(()) => harness::sandbox::Enforcement::Established,
            Err(err) => harness::sandbox::Enforcement::Failed {
                reason: err.to_string(),
            },
        },
    };

    match harness::sandbox::reentry_action(&decision, &enforcement) {
        harness::sandbox::ReentryAction::Exec => exec_pending_command(&command, &cwd),
        harness::sandbox::ReentryAction::Refuse { reason, exit_code } => {
            tracing::error!("refusing sandbox re-entry: {reason}");
            eprintln!("{}", harness::sandbox::refusal_message(&reason));
            std::process::exit(exit_code)
        }
    }
}

/// Canonicalize the working directory handed over by the spawn side.
///
/// Landlock anchors its rules on resolved inodes and the pending command inherits this
/// directory, so a relative or symlinked root (`.`, `~/wip`) is resolved once, here,
/// instead of being re-derived differently on each end of the protocol.
///
/// Documented fallback: if canonicalization fails (the directory vanished
/// concurrently, a component is unreadable), the raw path is kept and the failure is
/// reported on stderr. That is not a fail-open — the sandbox either resolves that path
/// itself or the attempt fails, and a failed attempt exits
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

/// Exec `sh -c <command>` in `cwd`. Reached only for a
/// [`harness::sandbox::ReentryAction::Exec`].
fn exec_pending_command(command: &str, cwd: &std::path::Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // `exec()` only returns when it failed, i.e. the process is still unsandboxed
        // or about to be — so it terminates the process instead of continuing.
        let err = std::process::Command::new("sh")
            .arg("-c")
            .arg(command)
            .current_dir(cwd)
            .exec();
        eprintln!("Failed to exec shell in sandbox: {err}");
        std::process::exit(1)
    }
    #[cfg(not(unix))]
    {
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
            "-h" | "--help" => {
                println!(
                    "marmel — autonomous agentic coding assistant\n\n\
                     USAGE:\n    marmel [--config <path>] [--raw] [--debug] [--resume | --fresh] [PROMPT]\n\n\
                     FLAGS:\n    --raw            force headless stdout (pipe-friendly) mode\n\
                     --debug          log all incoming and outgoing LLM and tool traffic to debug.log\n\
                     --config <path>  override the config file path\n\
                     --resume         rehydrate the previous run from .marmel/session/\n\
                     --fresh          start fresh even if session_resume / MARMEL_RESUME is set\n\
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

/// The session-flag usage errors, resolved once in `main` after `config::load`
/// (flow §3.3). Two cases, both the same shape as the valueless `--config`
/// precedent (`eprintln!` + `exit(2)`):
///
/// * `--resume` **and** `--fresh` — contradictory requests;
/// * `--resume` while capture is disabled — resuming a bundle this run would
///   never have written is not a meaningful state. `cfg.session_enabled` already
///   folds in `MARMEL_SESSION_DISABLE` (see [`config::apply_session_env`]).
///
/// `--fresh` alone is never an error: it is the documented escape hatch, and it
/// also overrides `session_resume = true` / `MARMEL_RESUME=1`. The boot-time
/// restore these switches feed is wired (t-012/t-024): the handoff at
/// `ui::session::set_resume_request` makes the session loop boot from the previous
/// run's bundle, and a refused restore exits with `session::EXIT_RESTORE_REFUSED`.
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
        // t-024 (flow §1.8, T-8): flush the session bundle before the panic
        // message is printed, so the last thing a crashed run leaves behind is a
        // bundle that says `panicHook` rather than one still claiming
        // `inProgress`. Three properties this hook must keep, and only keeps
        // because they are built into `commit_from_panic`:
        //  * pure synchronous I/O — no runtime is built here (a runtime cannot be
        //    created while a runtime is unwinding, and none is needed);
        //  * lock-poison-tolerant — every guard on that path takes
        //    `into_inner()` on a poisoned lock (precedent: `manager::phase`);
        //  * never blocks — the session's `CommitLock` is only *tried*; if the
        //    panic happened inside a commit on the session thread, the hook
        //    degrades honestly (the bundle keeps `inProgress` and the next boot
        //    infers `HardKillInferred`) instead of deadlocking against the thread
        //    that is unwinding. A panic on any other thread takes the same path
        //    and reports exactly what it managed to write.
        // `ui::restore()` above is untouched and still runs first: the screen is
        // restored whether or not the flush succeeds, and the flush itself cannot
        // panic out of the hook because it is wrapped here too.
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
    /// of the literal in `main.rs` is what let them drift (t-052 defect X3).
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

    // ------------------------------------------------------------- session flags

    #[test]
    fn session_flags_default_to_absent() {
        let args = CliArgs::default();
        assert!(!args.resume);
        assert!(!args.fresh);
    }

    /// No flags (the pre-t-007 invocation) must produce no error, whatever the
    /// config says — the run behaves exactly as before.
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
        for (flag, field) in [("\"--resume\"", "resume"), ("\"--fresh\"", "fresh")] {
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
}
