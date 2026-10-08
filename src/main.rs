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
    /// Optional initial prompt to start the session.
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
    config::set_active(cfg.clone());

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

    // Boot MCP servers if configured
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
            "-h" | "--help" => {
                println!(
                    "marmel — autonomous agentic coding assistant\n\n\
                     USAGE:\n    marmel [--config <path>] [--raw] [--debug] [PROMPT]\n\n\
                     FLAGS:\n    --raw            force headless stdout (pipe-friendly) mode\n\
                     --debug          log all incoming and outgoing LLM and tool traffic to debug.log\n\
                     --config <path>  override the config file path\n\
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
    #[cfg(unix)]
    #[test]
    fn test_canonical_sandbox_cwd_resolves_symlinks() {
        let temp = tempfile::tempdir().unwrap();
        let real = temp.path().join("real");
        std::fs::create_dir_all(&real).unwrap();
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();

        assert_eq!(canonical_sandbox_cwd(&link.to_string_lossy()), real);
    }

    /// Documented fallback: an unresolvable path is kept as given (and reported), and
    /// the sandbox attempt then decides — failure is fail-closed, not a widening.
    #[test]
    fn test_canonical_sandbox_cwd_falls_back_to_the_raw_path() {
        let raw = "/definitely/not/a/directory/marmel-t052";
        assert_eq!(canonical_sandbox_cwd(raw), std::path::PathBuf::from(raw));
    }
}
