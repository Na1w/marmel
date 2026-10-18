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
    // Hand the CLI's resume decision to the module that owns the session loop. The
    // precedence `--fresh` > `--resume` > `MARMEL_RESUME` / `session_resume` is
    // documented at `ui::session::resume_requested`, and the contradictory
    // combinations are rejected above (`exit 2`). With neither flag this records
    // `false`/`false`.
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

    // One honest line about the shared HTTP client before anything needs it — the
    // MCP boot, the Manager and every session turn below all talk to a backend
    // through it. A host with no TLS trust store makes that client impossible to
    // build; the client then carries the verdict instead of panicking, so this is
    // a warning about a run whose every LLM turn will fail, not a new verdict:
    // the process keeps the exit code its session outcome produces.
    report_http_client_verdict(use_raw);

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
/// pending command — **only if enforcement was actually established**. The order is
/// load-bearing: resolve the cwd canonically ([`canonical_sandbox_cwd`]); honour the
/// explicit opt-out ([`harness::pty::SANDBOX_OPTOUT_ENV`]) *before* any Landlock call,
/// saying so out loud ([`harness::sandbox::optout_warning`]); otherwise attempt
/// enforcement; then ask [`harness::sandbox::reentry_action`], which on
/// [`harness::sandbox::ReentryAction::Refuse`] never runs the command unsandboxed:
/// stderr gets the reason; exit code [`harness::sandbox::SANDBOX_REFUSE_EXIT_CODE`] (non-zero).
fn run_sandbox_reentry(args: &mut impl Iterator<Item = String>) -> Result<()> {
    let cwd_raw = args.next().unwrap_or_else(|| ".".to_string());
    let command = args.next().unwrap_or_default();
    let cwd = canonical_sandbox_cwd(&cwd_raw);

    // The extra read-only paths configured by the operator arrive through the
    // environment: this process is the re-entry child, and it never loads a config
    // file, so the spawning process published them in the variable named by
    // harness::sandbox::SANDBOX_EXTRA_RO_PATHS_ENV. Absent means the built-in
    // allow-list only. They are read once, here, and handed to the enforcement call
    // below, which turns them into read-only Landlock rules.
    let extra_readonly_paths = harness::sandbox::extra_readonly_paths_from_env();
    tracing::debug!(
        count = extra_readonly_paths.len(),
        "sandbox re-entry: extra read-only paths received from the spawning process"
    );

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
        harness::sandbox::ReentryAction::Exec => exec_pending_command(&command, &cwd),
        harness::sandbox::ReentryAction::Refuse { reason, exit_code } => {
            tracing::error!("refusing sandbox re-entry: {reason}");
            eprintln!("{}", harness::sandbox::refusal_message(&reason));
            std::process::exit(exit_code)
        }
    }
}

/// Canonicalize the working directory handed over by the spawn side. Landlock anchors
/// its rules on resolved inodes and the pending command inherits this directory, so a
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
        eprintln!("error: failed to exec {program} under the Landlock sandbox: {err}.{cause}");
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
