//! Marmennill (marmel) — agentic coding assistant CLI entry point.

use anyhow::Result;
use marmennill::{agent, config, harness, llm, mcp, orchestrator, ui};

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
    if let Some(first) = raw_args.next()
        && first == "--internal-sandbox-exec"
    {
        let cwd = raw_args.next().unwrap_or_else(|| ".".to_string());
        let command = raw_args.next().unwrap_or_default();
        let _ = harness::sandbox::apply_sandbox(std::path::Path::new(&cwd));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            let err = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .exec();
            eprintln!("Failed to exec shell in sandbox: {err}");
            std::process::exit(1);
        }
        #[cfg(not(unix))]
        {
            let mut child = std::process::Command::new("sh")
                .arg("-c")
                .arg(&command)
                .current_dir(&cwd)
                .spawn()?;
            let status = child.wait()?;
            std::process::exit(status.code().unwrap_or(1));
        }
    }

    let args = parse_args();
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    harness::set_workspace_root(&cwd);

    let mut cfg = config::load(args.config.as_deref())?;
    if args.debug {
        cfg.debug = true;
    }

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

    if use_raw {
        rt.block_on(ui::raw::run(&cfg, args.prompt, manager))
    } else {
        rt.block_on(ui::tui::run(&cfg, args.prompt, manager))
    }
}

fn boot_manager(cfg: &config::Config) -> std::sync::Arc<orchestrator::OrchestratorManager> {
    let plan = agent::phase::Plan::default();
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
                tracing_subscriber::EnvFilter::try_from_default_env()
                    .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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
                    tracing_subscriber::EnvFilter::try_from_default_env()
                        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
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
}
