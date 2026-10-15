//! Configuration schema, TOML parsing, and path expansion for marmel.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Default delegation depth bound.
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 3;

/// Default reasoning/thinking token budget per single turn.
pub const DEFAULT_MAX_THINKING_TOKENS: usize = 32768;

/// Default per-harness-command timeout in seconds (`command_timeout_secs`).
pub const DEFAULT_COMMAND_TIMEOUT_SECS: u64 = 60;

/// Inclusive lower bound of a valid command timeout, in seconds.
pub const MIN_COMMAND_TIMEOUT_SECS: u64 = 1;

/// Inclusive upper bound of a valid command timeout, in seconds.
///
/// A configured value (or a per-call `timeout_seconds` override) outside
/// [`MIN_COMMAND_TIMEOUT_SECS`]..=[`MAX_COMMAND_TIMEOUT_SECS`] is clamped into
/// this range on read — it is never used verbatim, and never silently:
/// see [`effective_command_timeout_secs`].
pub const MAX_COMMAND_TIMEOUT_SECS: u64 = 300;

/// Default floor between throttled session commits
/// (`session_commit_min_interval_ms`, flow §3.1).
pub const DEFAULT_SESSION_COMMIT_MIN_INTERVAL_MS: u64 = 250;

/// Default cap on the in-memory per-worker mirror
/// (`session_worker_mirror_max_messages`, flow §1.3 R-TRIG-4).
pub const DEFAULT_SESSION_WORKER_MIRROR_MAX_MESSAGES: usize = 4000;

/// Default soft ceiling on total session-bundle member bytes
/// (`session_max_bundle_bytes`, flow §3.1; A §7.1 C1 warn-once latch).
pub const DEFAULT_SESSION_MAX_BUNDLE_BYTES: u64 = 32 * 1024 * 1024;

/// Environment key that requests a session resume for one run
/// (`1`/`true` ⇒ true). Equivalent to `--resume`; flow §3.2.
pub const SESSION_RESUME_ENV: &str = "MARMEL_RESUME";

/// Environment key that disables session capture. **One-way switch**: it can only
/// turn capture off, never on, mirroring how `MARMEL_DISABLE_SANDBOX` ORs with
/// [`Config::sandbox_disabled`]; flow §3.2.
pub const SESSION_DISABLE_ENV: &str = "MARMEL_SESSION_DISABLE";

/// Environment key that overrides the commit throttling floor (integer ms).
pub const SESSION_COMMIT_MIN_MS_ENV: &str = "MARMEL_SESSION_COMMIT_MIN_MS";

/// Config key that disables the Landlock sandbox, used verbatim in the
/// attributable opt-out label that [`crate::harness::pty::log_sandbox_decision`]
/// warns about (alongside the env fallback
/// [`crate::harness::pty::SANDBOX_OPTOUT_ENV`]).
pub const SANDBOX_DISABLED_KEY: &str = "sandbox_disabled";

/// Orchestration configuration parsed from marmel.toml.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct OrchestrationConfig {
    /// Fractal delegation depth bound. Default 3.
    pub max_recursion_depth: usize,
    /// Manager module path (e.g. `src/orchestrator/mod.rs`).
    pub manager_module: String,
    /// Specialists table: role id -> { module, tools: [...] }.
    pub specialists: BTreeMap<String, SpecialistConfig>,
    /// MCP servers whose tools the orchestrator is allowed to see.
    pub mcp_servers: Vec<String>,
}

impl OrchestrationConfig {
    pub fn default_depth() -> Self {
        Self {
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            ..Self::default()
        }
    }
}

/// Monitoring & Resilience configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MonitoringConfig {
    /// Whether the resilience harness is active.
    pub enabled: bool,
    /// Number of consecutive identical calls / alternating cycles that triggers an intervention.
    pub repetition_threshold: usize,
    /// Minimum pattern length (in characters) for text-repetition detection.
    pub min_pattern_len: usize,
    /// Maximum output tokens per single streaming turn before cutting off runaway generation.
    pub max_stream_tokens: usize,
    /// Maximum reasoning/thinking tokens per single streaming turn before cutting off runaway reasoning.
    pub max_thinking_tokens: usize,
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repetition_threshold: 5,
            min_pattern_len: 5,
            max_stream_tokens: 32768,
            max_thinking_tokens: DEFAULT_MAX_THINKING_TOKENS,
        }
    }
}

/// A single per-specialist configuration entry from `[orchestration.specialists]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct SpecialistConfig {
    pub module: String,
    /// Allowed tool namespaces.
    pub tools: Vec<String>,
    pub model: Option<String>,
    /// Optional backend URL override for this specialist.
    pub backend_url: Option<String>,
    /// Optional auth token override for this specialist.
    pub auth_token: Option<String>,
    /// Optional model override for this specialist's validator.
    pub validator_model: Option<String>,
    /// Optional backend URL override for this specialist's validator.
    pub validator_backend_url: Option<String>,
    /// Optional auth token override for this specialist's validator.
    pub validator_auth_token: Option<String>,
    /// Optional max validation iterations.
    pub max_validator_iterations: Option<usize>,
    /// Optional max fix-loop rounds per validation pass (defaults to MAX_FIX_LOOP_ROUNDS = 200).
    pub max_fix_loop_rounds: Option<usize>,
    /// Whether automated validation is enabled for this specialist (default: true).
    #[serde(alias = "auto_validate", alias = "enable_validation")]
    pub enable_validator: Option<bool>,
    /// Optional max thinking tokens override for this specialist.
    #[serde(alias = "reasoning_budget", alias = "thinking_budget")]
    pub max_thinking_tokens: Option<usize>,
    /// MCP servers whose tools this specialist is allowed to see.
    pub mcp_servers: Vec<String>,
}

/// Resolved runtime configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub backend_url: String,
    pub auth_token: String,
    pub model: String,
    pub temperature: f32,
    pub top_p: f32,
    pub frequency_penalty: f32,
    pub presence_penalty: f32,
    pub max_context_tokens: usize,
    pub system_prompt_path: PathBuf,
    pub preserve_thinking: bool,
    /// Timeout applied to each harness command (`run_command` / PTY execution)
    /// when the call itself does not pass a `timeout_seconds` override.
    /// Documented valid range: [`MIN_COMMAND_TIMEOUT_SECS`]..=
    /// [`MAX_COMMAND_TIMEOUT_SECS`] seconds (1..=300); out-of-range values are
    /// clamped into it when read (see [`effective_command_timeout_secs`]).
    /// Default [`DEFAULT_COMMAND_TIMEOUT_SECS`] (60 s).
    pub command_timeout_secs: u64,
    /// Disable the Landlock sandbox re-entry for harness commands.
    ///
    /// Default `false`: the sandbox is **on** and every command is re-entered
    /// through the resolved executable (fail-closed — see
    /// [`crate::harness::sandbox`], which refuses to exec with
    /// `SANDBOX_REFUSE_EXIT_CODE` when Landlock cannot be enforced).
    ///
    /// `true` is an explicit, attributable operator opt-out and is equivalent to
    /// the environment fallback `MARMEL_DISABLE_SANDBOX`
    /// ([`crate::harness::pty::SANDBOX_OPTOUT_ENV`]): **either** source opts out,
    /// and the skip is always warned about by
    /// [`crate::harness::pty::log_sandbox_decision`]. Read through
    /// [`sandbox_opt_out`].
    pub sandbox_disabled: bool,
    pub max_repetition_threshold: usize,
    pub enable_xml_rescue: bool,
    pub ui_mode: String,
    /// Detailed debug logging to debug.log.
    pub debug: bool,
    /// Maximum reasoning/thinking tokens per single turn before cutting off runaway reasoning.
    pub max_thinking_tokens: usize,
    /// Resilience harness thresholds.
    pub monitoring: Option<MonitoringConfig>,
    pub orchestration: OrchestrationConfig,
    /// Configured external MCP servers (`[mcp_servers.<name>]`).
    pub mcp_servers: HashMap<String, crate::mcp::McpServerConfig>,
    /// Optional base prompt appended to the orchestrator system prompt.
    pub base_prompt: Option<String>,
    /// Optional global cap on fix-loop rounds per validation pass (defaults to MAX_FIX_LOOP_ROUNDS = 200).
    pub max_fix_loop_rounds: Option<usize>,
    /// Session capture/rehydration master switch (Design A D2: replaces the
    /// deleted `enable_rehydration`). Default `true`: a bundle is always written.
    ///
    /// One-way against the environment: `MARMEL_SESSION_DISABLE` can turn capture
    /// **off**, never on — see [`session_env_overrides`].
    pub session_enabled: bool,
    /// Rehydrate from the bundle at boot. Default `false`: a fresh start is the
    /// default behaviour; `--resume` / `MARMEL_RESUME` turn it on for one run.
    pub session_resume: bool,
    /// Floor between throttled commits (flow §1.3 R-TRIG-2). Default 250 ms.
    pub session_commit_min_interval_ms: u64,
    /// Cap on the in-memory per-worker mirror (flow §1.3 R-TRIG-4). Default 4000 records.
    pub session_worker_mirror_max_messages: usize,
    /// Commit at every landed tool result (flow T-3). `false` keeps T-1/T-2/T-6/T-9
    /// and drops T-3/T-4: fewer writes, coarser crash granularity. Default `true`.
    pub session_commit_on_tool_result: bool,
    /// Soft ceiling on total member bytes before the committer stops writing
    /// throttled triggers and warns once. Default 32 MiB.
    pub session_max_bundle_bytes: u64,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend_url: "http://localhost:8000/v1".to_string(),
            auth_token: String::new(),
            model: "qwen-3.8-27b".to_string(),
            temperature: 0.7,
            top_p: 0.9,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
            max_context_tokens: 8192,
            system_prompt_path: PathBuf::from("prompts/system.md"),
            preserve_thinking: true,
            command_timeout_secs: DEFAULT_COMMAND_TIMEOUT_SECS,
            sandbox_disabled: false,
            max_repetition_threshold: 5,
            enable_xml_rescue: true,
            ui_mode: "tui".to_string(),
            debug: false,
            max_thinking_tokens: DEFAULT_MAX_THINKING_TOKENS,
            monitoring: Some(MonitoringConfig::default()),
            orchestration: OrchestrationConfig::default_depth(),
            mcp_servers: HashMap::new(),
            base_prompt: None,
            max_fix_loop_rounds: None,
            session_enabled: true,
            session_resume: false,
            session_commit_min_interval_ms: DEFAULT_SESSION_COMMIT_MIN_INTERVAL_MS,
            session_worker_mirror_max_messages: DEFAULT_SESSION_WORKER_MIRROR_MAX_MESSAGES,
            session_commit_on_tool_result: true,
            session_max_bundle_bytes: DEFAULT_SESSION_MAX_BUNDLE_BYTES,
        }
    }
}

static ACTIVE_CONFIG: std::sync::RwLock<Option<Config>> = std::sync::RwLock::new(None);

pub fn set_active(cfg: Config) {
    if let Ok(mut lock) = ACTIVE_CONFIG.write() {
        *lock = Some(cfg);
    }
}

/// Retrieve a clone of the globally active configuration, if set.
pub fn get_active() -> Option<Config> {
    ACTIVE_CONFIG.read().ok().and_then(|lock| lock.clone())
}

/// Drop the globally active configuration (test-only: restores the "no config
/// loaded" state so an installed config can never leak into another test).
#[cfg(test)]
pub(crate) fn clear_active() {
    if let Ok(mut lock) = ACTIVE_CONFIG.write() {
        *lock = None;
    }
}

/// The configured per-command timeout (seconds) of the active config, clamped
/// into the documented range [`MIN_COMMAND_TIMEOUT_SECS`]..=
/// [`MAX_COMMAND_TIMEOUT_SECS`]; [`DEFAULT_COMMAND_TIMEOUT_SECS`] when no config
/// is active.
///
/// An out-of-range configured value is **logged** (old → new) and clamped — it is
/// never applied verbatim and never silently ignored.
pub fn active_command_timeout_secs() -> u64 {
    let Some(cfg) = get_active() else {
        return DEFAULT_COMMAND_TIMEOUT_SECS;
    };
    let configured = cfg.command_timeout_secs;
    let applied = configured.clamp(MIN_COMMAND_TIMEOUT_SECS, MAX_COMMAND_TIMEOUT_SECS);
    if applied != configured {
        tracing::warn!(
            key = "command_timeout_secs",
            requested = configured,
            applied,
            valid_range = format!(
                "{}..={}",
                MIN_COMMAND_TIMEOUT_SECS, MAX_COMMAND_TIMEOUT_SECS
            ),
            "configured command timeout is out of range; clamped ({}s -> {}s)",
            configured,
            applied
        );
    }
    applied
}

/// The timeout applied to one harness command, in seconds.
///
/// An explicit per-call override (`timeout_seconds` / `timeout` on the tool call)
/// wins; otherwise the configured [`Config::command_timeout_secs`] is used. The
/// result is always inside the documented range, so neither a misconfigured file
/// nor a model-supplied override can produce an unusable timeout.
pub fn effective_command_timeout_secs(requested: Option<u64>) -> u64 {
    match requested {
        Some(requested) => {
            let applied = requested.clamp(MIN_COMMAND_TIMEOUT_SECS, MAX_COMMAND_TIMEOUT_SECS);
            if applied != requested {
                tracing::debug!(
                    requested,
                    applied,
                    "per-call timeout override is out of range; clamped ({requested}s -> {applied}s)"
                );
            }
            applied
        }
        None => active_command_timeout_secs(),
    }
}

/// `true` when the active config disables the Landlock sandbox
/// ([`Config::sandbox_disabled`]). Default `false` (sandbox on, fail-closed).
pub fn sandbox_disabled() -> bool {
    get_active().is_some_and(|cfg| cfg.sandbox_disabled)
}

/// Merge an environment-derived sandbox opt-out with the typed config knob.
///
/// Pure and injectable: `env_opt_out` is whatever the caller parsed from
/// [`crate::harness::pty::SANDBOX_OPTOUT_ENV`], and `config_disabled` is the
/// resolved [`Config::sandbox_disabled`]. **Either source opts out**; when both
/// are set the env value keeps attribution (it names the variable that was set),
/// which is why the two are ordered this way and never AND-ed.
pub fn merge_sandbox_opt_out(
    env_opt_out: crate::harness::pty::OptOut,
    config_disabled: bool,
) -> crate::harness::pty::OptOut {
    use crate::harness::pty::OptOut;
    match env_opt_out {
        // Already an explicit, attributable env opt-out: keep it as-is.
        opt_out @ OptOut::Explicit { .. } => opt_out,
        OptOut::None if config_disabled => OptOut::Explicit {
            setting: format!("{SANDBOX_DISABLED_KEY}=true"),
        },
        OptOut::None => OptOut::None,
    }
}

/// The opt-out fed into [`crate::harness::pty::SandboxInputs::opt_out`]: the
/// env-derived opt-out (still parsed by the harness, so it keeps working on its
/// own) layered with the typed config knob from the active config.
pub fn sandbox_opt_out(env_opt_out: crate::harness::pty::OptOut) -> crate::harness::pty::OptOut {
    merge_sandbox_opt_out(env_opt_out, sandbox_disabled())
}

/// Config lookup order: CLI --config > ./.marmel.toml > ~/.config/marmel/config.toml > env vars > defaults.
pub fn load(explicit_path: Option<&str>) -> Result<Config> {
    let mut cfg = Config::default();
    if let Some(path) = resolve_config_path(explicit_path) {
        let file = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        cfg = merge(cfg, toml::from_str::<PartialConfig>(&file)?);
    }

    if cfg.auth_token.is_empty()
        && let Ok(token) = std::env::var("MARMEL_AUTH_TOKEN")
    {
        cfg.auth_token = token;
    }
    if let Ok(url) = std::env::var("MARMEL_BACKEND_URL")
        && !url.trim().is_empty()
    {
        cfg.backend_url = url;
    }
    if let Ok(m) = std::env::var("MARMEL_MODEL")
        && !m.trim().is_empty()
    {
        cfg.model = m;
    }

    // Session keys (flow §3.2): env beats file beats default, with the deliberate
    // exception that `MARMEL_SESSION_DISABLE` is a one-way switch. Factored into
    // [`apply_session_env`] so the precedence is testable without mutating the
    // process environment.
    cfg = apply_session_env(
        cfg,
        std::env::var(SESSION_RESUME_ENV).ok(),
        std::env::var(SESSION_DISABLE_ENV).ok(),
        std::env::var(SESSION_COMMIT_MIN_MS_ENV).ok(),
    );

    cfg.expand_paths();
    Ok(cfg)
}

/// Tri-state reading of a `MARMEL_*` boolean: `1`/`true`/`yes`/`on` ⇒ `Some(true)`,
/// `0`/`false`/`no`/`off` ⇒ `Some(false)`, anything else (including unset and
/// unparseable junk) ⇒ `None`, which means "leave the file/default value alone".
pub fn env_flag(value: Option<&String>) -> Option<bool> {
    let raw = value.as_ref().map(|v| v.trim().to_ascii_lowercase())?;
    match raw.as_str() {
        "1" | "true" | "yes" | "on" => Some(true),
        "0" | "false" | "no" | "off" => Some(false),
        _ => None,
    }
}

/// Applies the three session environment overrides (flow §3.2) to `cfg`.
///
/// * `MARMEL_RESUME` — resume for this run; a recognized value overrides the file,
///   an unrecognized one is reported and ignored.
/// * `MARMEL_SESSION_DISABLE` — **one-way**: `true` turns capture off, and nothing
///   this function does can turn it back on. Capture-on is the safety property the
///   whole design rests on, so an env var must not override an explicit
///   `session_enabled = false` in the file in the other direction.
/// * `MARMEL_SESSION_COMMIT_MIN_MS` — integer ms floor; a non-integer is reported
///   and ignored rather than silently treated as 0.
pub fn apply_session_env(
    mut cfg: Config,
    resume: Option<String>,
    disable: Option<String>,
    min_ms: Option<String>,
) -> Config {
    match env_flag(resume.as_ref()) {
        Some(value) => cfg.session_resume = value,
        None if resume.is_some() => {
            tracing::warn!(
                "{SESSION_RESUME_ENV}={:?} is not a recognized boolean; keeping session_resume={}",
                resume,
                cfg.session_resume
            );
        }
        None => {}
    }
    if matches!(env_flag(disable.as_ref()), Some(true)) {
        cfg.session_enabled = false;
    } else if disable.is_some() && env_flag(disable.as_ref()).is_none() {
        tracing::warn!(
            "{SESSION_DISABLE_ENV}={:?} is not a recognized boolean; capture stays enabled={}",
            disable,
            cfg.session_enabled
        );
    }
    if let Some(raw) = min_ms.as_deref() {
        match raw.trim().parse::<u64>() {
            Ok(value) => cfg.session_commit_min_interval_ms = value,
            Err(err) => tracing::warn!(
                "{SESSION_COMMIT_MIN_MS_ENV}={raw:?} is not an integer number of milliseconds \
                 ({err}); keeping session_commit_min_interval_ms={}",
                cfg.session_commit_min_interval_ms
            ),
        }
    }
    cfg
}

fn resolve_config_path(explicit_path: Option<&str>) -> Option<PathBuf> {
    if let Some(p) = explicit_path {
        return Some(PathBuf::from(p));
    }
    if let Ok(cwd) = std::env::current_dir() {
        for name in &[
            "marmel.toml",
            ".marmel.toml",
            ".marmel/marmel.toml",
            ".marmel/config.toml",
        ] {
            let candidate = cwd.join(name);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    if let Some(home) = home_dir() {
        for path in &[
            ".marmel/marmel.toml",
            ".marmel/config.toml",
            ".config/marmel/config.toml",
            ".config/marmel/marmel.toml",
        ] {
            let candidate = home.join(path);
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }
    None
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PartialConfig {
    pub backend_url: Option<String>,
    pub auth_token: Option<String>,
    pub model: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub presence_penalty: Option<f32>,
    pub max_context_tokens: Option<usize>,
    pub system_prompt_path: Option<PathBuf>,
    pub preserve_thinking: Option<bool>,
    pub command_timeout_secs: Option<u64>,
    pub sandbox_disabled: Option<bool>,
    pub max_repetition_threshold: Option<usize>,
    pub enable_xml_rescue: Option<bool>,
    pub max_thinking_tokens: Option<usize>,
    pub ui_mode: Option<String>,
    pub monitoring: Option<PartialMonitoringConfig>,
    pub orchestration: Option<PartialOrchestrationConfig>,
    #[serde(default)]
    pub mcp_servers: HashMap<String, crate::mcp::McpServerConfig>,
    pub base_prompt: Option<String>,
    pub session_enabled: Option<bool>,
    pub session_resume: Option<bool>,
    pub session_commit_min_interval_ms: Option<u64>,
    pub session_worker_mirror_max_messages: Option<usize>,
    pub session_commit_on_tool_result: Option<bool>,
    pub session_max_bundle_bytes: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PartialMonitoringConfig {
    pub enabled: Option<bool>,
    pub repetition_threshold: Option<usize>,
    pub min_pattern_len: Option<usize>,
    pub max_stream_tokens: Option<usize>,
    pub max_thinking_tokens: Option<usize>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PartialOrchestrationConfig {
    pub max_recursion_depth: Option<usize>,
    pub manager_module: Option<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub specialists: HashMap<String, SpecialistConfig>,
}

fn merge(mut base: Config, partial: PartialConfig) -> Config {
    if let Some(v) = partial.backend_url
        && !v.is_empty()
    {
        base.backend_url = v;
    }
    if let Some(v) = partial.auth_token
        && !v.is_empty()
    {
        base.auth_token = v;
    }
    if let Some(v) = partial.model
        && !v.is_empty()
    {
        base.model = v;
    }
    if let Some(v) = partial.ui_mode
        && !v.is_empty()
    {
        base.ui_mode = v;
    }
    if let Some(v) = partial.system_prompt_path
        && !v.as_os_str().is_empty()
    {
        base.system_prompt_path = v;
    }
    if let Some(v) = partial.temperature {
        base.temperature = v;
    }
    if let Some(v) = partial.top_p {
        base.top_p = v;
    }
    if let Some(v) = partial.frequency_penalty {
        base.frequency_penalty = v;
    }
    if let Some(v) = partial.presence_penalty {
        base.presence_penalty = v;
    }
    if let Some(v) = partial.max_context_tokens {
        base.max_context_tokens = v;
    }
    if let Some(v) = partial.preserve_thinking {
        base.preserve_thinking = v;
    }
    if let Some(v) = partial.command_timeout_secs {
        base.command_timeout_secs = v;
    }
    if let Some(v) = partial.sandbox_disabled {
        base.sandbox_disabled = v;
    }
    if let Some(v) = partial.max_repetition_threshold {
        base.max_repetition_threshold = v;
    }
    if let Some(v) = partial.enable_xml_rescue {
        base.enable_xml_rescue = v;
    }
    if let Some(v) = partial.max_thinking_tokens {
        base.max_thinking_tokens = v;
    }
    if let Some(v) = partial.base_prompt {
        base.base_prompt = Some(v);
    }
    if let Some(v) = partial.session_enabled {
        base.session_enabled = v;
    }
    if let Some(v) = partial.session_resume {
        base.session_resume = v;
    }
    if let Some(v) = partial.session_commit_min_interval_ms {
        base.session_commit_min_interval_ms = v;
    }
    if let Some(v) = partial.session_worker_mirror_max_messages {
        base.session_worker_mirror_max_messages = v;
    }
    if let Some(v) = partial.session_commit_on_tool_result {
        base.session_commit_on_tool_result = v;
    }
    if let Some(v) = partial.session_max_bundle_bytes {
        base.session_max_bundle_bytes = v;
    }

    if let Some(p_mon) = partial.monitoring {
        let base_mon = base
            .monitoring
            .get_or_insert_with(MonitoringConfig::default);
        if let Some(e) = p_mon.enabled {
            base_mon.enabled = e;
        }
        if let Some(t) = p_mon.repetition_threshold {
            base_mon.repetition_threshold = t;
        }
        if let Some(l) = p_mon.min_pattern_len {
            base_mon.min_pattern_len = l;
        }
        if let Some(s) = p_mon.max_stream_tokens {
            base_mon.max_stream_tokens = s;
        }
        if let Some(th) = p_mon.max_thinking_tokens {
            base_mon.max_thinking_tokens = th;
        }
    }

    if let Some(p_orch) = partial.orchestration {
        if let Some(d) = p_orch.max_recursion_depth {
            base.orchestration.max_recursion_depth = d;
        }
        if let Some(m) = p_orch.manager_module
            && !m.is_empty()
        {
            base.orchestration.manager_module = m;
        }
        if !p_orch.specialists.is_empty() {
            base.orchestration.specialists.extend(p_orch.specialists);
        }
        if !p_orch.mcp_servers.is_empty() {
            base.orchestration.mcp_servers = p_orch.mcp_servers;
        }
    }

    if !partial.mcp_servers.is_empty() {
        base.mcp_servers.extend(partial.mcp_servers);
    }

    base
}

impl Config {
    fn expand_paths(&mut self) {
        if let Some(home) = home_dir() {
            let p = &self.system_prompt_path;
            self.system_prompt_path = if let Ok(stripped) = p.strip_prefix("~") {
                home.join(stripped)
            } else if p.is_relative() {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(p)
            } else {
                p.to_path_buf()
            };
        }
    }
}

pub(crate) fn home_dir() -> Option<PathBuf> {
    // `HOME` wins when it is set. The fallback is written as an early return and
    // not as an `or_else` closure: on non-unix targets the closure body degenerates
    // into a plain `None`, which makes clippy raise
    // `clippy::unnecessary_lazy_evaluations` on the windows target only
    // (`docs/cross_platform_gating.md:94`, `docs/nonlinux_unused_inventory.md:47`),
    // and a cross-target `-D warnings` gate must stay green. Semantics are
    // unchanged: `HOME` first, platform fallback second.
    if let Some(home) = std::env::var_os("HOME") {
        return Some(PathBuf::from(home));
    }

    #[cfg(unix)]
    {
        // SAFETY: `getpwuid` returns a pointer to a libc-owned static passwd entry
        // for the real uid, or null when there is no such entry; the pointee is read
        // immediately and no other passwd call happens in between.
        let pw = unsafe { libc::getpwuid(libc::getuid()) };
        if pw.is_null() {
            return None;
        }
        let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
        Some(PathBuf::from(dir.to_string_lossy().into_owned()))
    }
    #[cfg(not(unix))]
    {
        // No passwd database to fall back on: `HOME` is the only source.
        None
    }
}

/// Test-only serialization lock for **every** test in this binary that touches a
/// process global: the `MARMEL_DISABLE_SANDBOX` environment key (written through
/// `tests::SandboxEnvGuard`) and [`ACTIVE_CONFIG`] (written through
/// `tests::ActiveConfigTestGuard`, [`set_active`] and [`clear_active`]).
///
/// Rust 2024 makes `std::env::set_var`/`remove_var` `unsafe` precisely because a
/// concurrent env read on another thread races with them, and libtest runs tests
/// concurrently by default — CI never passes `--test-threads=1`
/// (`.github/workflows/ci.yml`; see `docs/windows_failures_analysis.md` "Site 1",
/// measured 33/40 failures in parallel vs 0/5 serialized). This lock, not the
/// thread count, is the gate that makes those writes sound.
///
/// `pub(crate)` so the other test modules that mutate `ACTIVE_CONFIG`
/// (`src/orchestrator/notice_tests.rs`, `src/harness/dispatch_integrity_tests.rs`)
/// can take the same lock.
#[cfg(test)]
pub(crate) static SANDBOX_ENV_TEST_SERIAL: std::sync::Mutex<()> =
    const { std::sync::Mutex::new(()) };

/// Take [`SANDBOX_ENV_TEST_SERIAL`] for the remainder of the calling test.
///
/// Not reentrant: a test must hold it exactly once. Both
/// `tests::SandboxEnvGuard` and `tests::ActiveConfigTestGuard` take it, so one test
/// must not construct both — drive the second concern through a pure seam instead
/// (`crate::harness::sandbox::opt_out_from_value`, [`merge_sandbox_opt_out`]).
#[cfg(test)]
#[must_use = "the guard is what serializes the process-global test state; hold it for the whole test"]
pub(crate) fn sandbox_env_test_lock() -> std::sync::MutexGuard<'static, ()> {
    // Poisoning is swallowed on purpose: one failing assertion must not cascade
    // into every later config test of this binary.
    //
    // `try_lock` in a bounded spin instead of a blocking `lock()` so that an
    // accidental second acquisition on the same thread (the lock is not
    // reentrant) fails loudly instead of hanging the whole test binary. The
    // budget (~1 s) is orders of magnitude above any real wait: the sections
    // guarded by this lock assign a `Config` and set or remove one env key.
    for attempt in 0..1_000_u32 {
        match SANDBOX_ENV_TEST_SERIAL.try_lock() {
            Ok(guard) => return guard,
            Err(std::sync::TryLockError::Poisoned(poisoned)) => return poisoned.into_inner(),
            Err(std::sync::TryLockError::WouldBlock) => {
                if attempt < 500 {
                    std::thread::yield_now();
                } else {
                    std::thread::sleep(std::time::Duration::from_millis(1));
                }
            }
        }
    }
    panic!(
        "`SANDBOX_ENV_TEST_SERIAL` stayed busy for a full second: a test almost \
         certainly took the (non-reentrant) process-global lock twice. Keep exactly \
         one guard chain per test and route the other concern through a pure seam \
         (`harness::sandbox::opt_out_from_value` / `config::merge_sandbox_opt_out`).",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_orchestr_config_default_depth() {
        let cfg = Config::default();
        assert_eq!(
            cfg.orchestration.max_recursion_depth,
            DEFAULT_MAX_RECURSION_DEPTH
        );
        assert!(cfg.orchestration.manager_module.is_empty());
        assert!(cfg.orchestration.specialists.is_empty());
    }

    #[test]
    fn test_orchestr_config_parses_specialists_table() {
        let toml_str = r#"
            backend_url = "http://localhost:9000/v1"
            [orchestration]
            max_recursion_depth = 4
            manager_module = "src/orchestrator/mod.rs"

            [orchestration.specialists]
            coder = { module = "src/agents/coder.rs", tools = ["delegate_task", "cli_*", "terminal__*", "kiwix__*"] }
            validator = { module = "src/agents/validator.rs", tools = ["delegate_task", "terminal__*", "gedcom__*"] }
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);

        assert_eq!(cfg.orchestration.max_recursion_depth, 4);
        assert_eq!(cfg.orchestration.manager_module, "src/orchestrator/mod.rs");
        assert_eq!(cfg.orchestration.specialists.len(), 2);

        let coder = cfg
            .orchestration
            .specialists
            .get("coder")
            .expect("coder present");
        assert_eq!(coder.module, "src/agents/coder.rs");
        assert!(coder.tools.iter().any(|t| t == "cli_*"));
        assert!(coder.tools.iter().any(|t| t == "terminal__*"));
        assert!(coder.tools.iter().any(|t| t == "kiwix__*"));

        let validator = cfg.orchestration.specialists.get("validator").unwrap();
        assert!(validator.tools.iter().any(|t| t == "gedcom__*"));
    }

    #[test]
    fn orchestration_absent_keeps_default() {
        let toml_str = "backend_url = \"http://localhost:9000/v1\"\n";
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);
        assert_eq!(
            cfg.orchestration.max_recursion_depth,
            DEFAULT_MAX_RECURSION_DEPTH
        );
        assert!(cfg.orchestration.specialists.is_empty());
    }

    #[test]
    fn monitoring_block_parses_and_merges() {
        let toml_str = r#"
            backend_url = "http://localhost:9000/v1"
            [monitoring]
            enabled = true
            repetition_threshold = 5
            min_pattern_len = 7
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);

        let mon = cfg.monitoring.expect("monitoring block present");
        assert!(mon.enabled);
        assert_eq!(mon.repetition_threshold, 5);
        assert_eq!(mon.min_pattern_len, 7);
    }

    #[test]
    fn test_mcp_servers_parse_and_merge() {
        let toml_str = r#"
            [mcp_servers.fs]
            command = "npx"
            args = ["-y", "@modelcontextprotocol/server-filesystem", "/tmp"]
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);
        assert!(cfg.mcp_servers.contains_key("fs"));
        assert_eq!(cfg.mcp_servers["fs"].command.as_deref(), Some("npx"));
    }

    #[test]
    fn test_mcp_servers_remote_url_parse_and_merge() {
        let toml_str = r#"
            [mcp_servers.remote]
            url = "https://example.com/mcp"
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);
        assert!(cfg.mcp_servers.contains_key("remote"));
        assert_eq!(
            cfg.mcp_servers["remote"].url.as_deref(),
            Some("https://example.com/mcp")
        );
        assert!(cfg.mcp_servers["remote"].command.is_none());
    }

    #[test]
    fn specialist_mcp_servers_parse() {
        let toml_str = r#"
            [orchestration.specialists.coder]
            module = "src/agents/coder.rs"
            mcp_servers = ["fs", "db"]
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);

        let coder = cfg
            .orchestration
            .specialists
            .get("coder")
            .expect("coder present");
        assert_eq!(coder.mcp_servers, vec!["fs".to_string(), "db".to_string()]);
    }

    #[test]
    fn orchestration_mcp_servers_parse() {
        let toml_str = r#"
            [orchestration]
            mcp_servers = ["fs"]
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);

        assert_eq!(cfg.orchestration.mcp_servers, vec!["fs".to_string()]);
    }

    #[test]
    fn test_set_and_get_active_config() {
        let custom = Config {
            model: "test-custom-model-123".to_string(),
            backend_url: "http://custom:1234/v1".to_string(),
            ..Default::default()
        };
        // The guard both serializes the write against the other tests that use
        // `ACTIVE_CONFIG` and restores the previous state afterwards, so this test
        // can neither race with nor leak into its neighbours.
        let _guard = ActiveConfigTestGuard::install(custom.clone());

        let retrieved = get_active().expect("active config should be present");
        assert_eq!(retrieved.model, "test-custom-model-123");
        assert_eq!(retrieved.backend_url, "http://custom:1234/v1");
    }

    #[test]
    fn test_monitoring_stream_and_thinking_tokens_parse() {
        let toml_str = r#"
            max_thinking_tokens = 16384

            [monitoring]
            enabled = true
            max_stream_tokens = 32768
            max_thinking_tokens = 8192
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);

        assert_eq!(cfg.max_thinking_tokens, 16384);
        let mon = cfg.monitoring.expect("monitoring present");
        assert_eq!(mon.max_stream_tokens, 32768);
        assert_eq!(mon.max_thinking_tokens, 8192);
    }

    #[test]
    fn test_base_prompt_parses_and_merges() {
        let toml_str = r#"
            backend_url = "http://localhost:9000/v1"
            base_prompt = "Do not schedule tasks in parallel on this machine."
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);

        assert_eq!(
            cfg.base_prompt.as_deref(),
            Some("Do not schedule tasks in parallel on this machine.")
        );
    }

    #[test]
    fn test_base_prompt_default_is_none() {
        let toml_str = r#"
            backend_url = "http://localhost:9000/v1"
        "#;
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        let cfg = merge(Config::default(), partial);

        assert!(cfg.base_prompt.is_none());
    }

    // t-035d — typed knobs that the harness actually reads:
    //   * `command_timeout_secs` used to be parsed and merged but never read, so
    //     every command ran with the built-in default;
    //   * the sandbox had no typed knob at all (env-only opt-out).
    // These tests are pty-free: the sandbox outcome is asserted through
    // `should_apply_sandbox(&SandboxInputs)`, never by executing a command.

    /// Install a config as the process-global active config and restore the
    /// previous state (including "none active") when the guard is dropped, so a
    /// failing assertion cannot leak config state into another test.
    ///
    /// `ACTIVE_CONFIG` is process-global, so the guard also holds
    /// [`SANDBOX_ENV_TEST_SERIAL`] for the whole test: without it a sibling test's
    /// `install` can replace the config between `set_active` and the assertion
    /// (libtest runs tests concurrently and CI does not serialize them).
    struct ActiveConfigTestGuard {
        /// Dropped after `Drop::drop` runs, so the lock is held while the previous
        /// config is restored.
        _serial: std::sync::MutexGuard<'static, ()>,
        previous: Option<Config>,
    }

    impl ActiveConfigTestGuard {
        fn install(cfg: Config) -> Self {
            let serial = sandbox_env_test_lock();
            let previous = get_active();
            set_active(cfg);
            Self {
                _serial: serial,
                previous,
            }
        }
    }

    impl Drop for ActiveConfigTestGuard {
        fn drop(&mut self) {
            match self.previous.take() {
                Some(cfg) => set_active(cfg),
                None => clear_active(),
            }
        }
    }

    /// Set or clear `MARMEL_DISABLE_SANDBOX` for one test and restore the state
    /// that was there before — including "unset" — on drop.
    ///
    /// The previous state is restored with `set_var`/`remove_var` (never with
    /// `set_var(key, "")`): on Windows an empty value *is* a delete
    /// (`SetEnvironmentVariableW`), so an "empty restore" would silently turn into
    /// a delete and leak a changed environment into the next test.
    struct SandboxEnvGuard {
        /// Held for the whole test. This is what makes the `unsafe` env writes
        /// below sound: no other test in this binary can read or write the key
        /// while it is mutated.
        _serial: std::sync::MutexGuard<'static, ()>,
        previous: Option<std::ffi::OsString>,
    }

    impl SandboxEnvGuard {
        /// Set the key to a non-empty value for one test.
        ///
        /// The empty value is rejected on purpose: it is indistinguishable from
        /// "unset" on Windows, so the "present but empty is not an opt-out"
        /// contract must be asserted through the pure parser
        /// (`crate::harness::sandbox::opt_out_from_value(Some(""))`).
        fn set(value: &str) -> Self {
            assert!(
                !value.is_empty(),
                "an empty env value is not representable on Windows — assert it through \
                 crate::harness::sandbox::opt_out_from_value(Some(\"\")) instead"
            );
            Self::with_env(Some(value))
        }

        /// Remove the key for one test (the "env absent" case), so a test can never
        /// depend on an ambient value inherited from the runner or from an earlier
        /// test.
        fn clear() -> Self {
            Self::with_env(None)
        }

        fn with_env(value: Option<&str>) -> Self {
            let serial = sandbox_env_test_lock();
            let previous = std::env::var_os(crate::harness::pty::SANDBOX_OPTOUT_ENV);
            // SAFETY: the invariant is process-global test state, not thread count.
            // `SANDBOX_ENV_TEST_SERIAL` is held from just before this write until
            // after the enclosing test's last assertion (and until the restore in
            // `Drop`), and every test in this binary that reads or writes this key
            // takes the same lock, so no env read can race with this mutation.
            unsafe {
                match value {
                    Some(value) => {
                        std::env::set_var(crate::harness::pty::SANDBOX_OPTOUT_ENV, value);
                    }
                    None => {
                        std::env::remove_var(crate::harness::pty::SANDBOX_OPTOUT_ENV);
                    }
                }
            }
            Self {
                _serial: serial,
                previous,
            }
        }
    }

    impl Drop for SandboxEnvGuard {
        fn drop(&mut self) {
            // SAFETY: `SANDBOX_ENV_TEST_SERIAL` is still held — the `_serial` field
            // drops after this body runs — so this restore cannot race with another
            // test's read or write of the key. `previous == None` means "the key was
            // absent", which is restored by removing it again.
            unsafe {
                match self.previous.take() {
                    Some(previous) => {
                        std::env::set_var(crate::harness::pty::SANDBOX_OPTOUT_ENV, previous);
                    }
                    None => {
                        std::env::remove_var(crate::harness::pty::SANDBOX_OPTOUT_ENV);
                    }
                }
            }
        }
    }

    /// The decision the harness would take for `opt_out` — pure logic, no PTY,
    /// no Landlock probe, no `current_exe()`.
    fn sandbox_decision_for(
        opt_out: crate::harness::pty::OptOut,
    ) -> crate::harness::pty::SandboxDecision {
        use crate::harness::pty::SandboxInputs;
        let exe = std::path::PathBuf::from("/usr/local/bin/marmel-dev");
        let inputs = SandboxInputs {
            landlock_supported: true,
            test_harness_exe: false,
            resolved_exe: Some(exe.as_path()),
            opt_out,
        };
        crate::harness::pty::should_apply_sandbox(&inputs)
    }

    fn partial_from(toml_str: &str) -> Config {
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        merge(Config::default(), partial)
    }

    #[test]
    fn sandbox_disabled_knob_defaults_to_false() {
        assert!(
            !Config::default().sandbox_disabled,
            "the sandbox must default to ON (fail-closed)"
        );
        assert!(
            !partial_from("backend_url = \"http://localhost:9000/v1\"\n").sandbox_disabled,
            "an absent `sandbox_disabled` keeps the default"
        );
        assert!(
            !partial_from("sandbox_disabled = false\n").sandbox_disabled,
            "an explicit false stays false"
        );
        // "No active config" is a process-global state, so it is taken and restored
        // under the same lock the config guards use.
        let _serial = sandbox_env_test_lock();
        let previous = get_active();
        clear_active();
        let observed = sandbox_disabled();
        if let Some(cfg) = previous {
            set_active(cfg);
        }
        assert!(!observed, "no active config must never disable the sandbox");
    }

    #[test]
    fn sandbox_disabled_knob_parses_from_toml() {
        let cfg = partial_from("sandbox_disabled = true\n");
        assert!(cfg.sandbox_disabled, "`sandbox_disabled = true` parses");

        // It also parses in a realistic file, next to the other knobs, without
        // disturbing them.
        let cfg = partial_from(
            r#"
                backend_url = "http://localhost:9000/v1"
                command_timeout_secs = 90
                sandbox_disabled = true

                [monitoring]
                enabled = true
            "#,
        );
        assert!(cfg.sandbox_disabled);
        assert_eq!(cfg.command_timeout_secs, 90);
        assert_eq!(cfg.backend_url, "http://localhost:9000/v1");
    }

    /// (a) the knob reaches `SandboxInputs::opt_out` and therefore the decision.
    #[test]
    fn sandbox_disabled_knob_reaches_sandbox_inputs_opt_out() {
        let cfg = partial_from("sandbox_disabled = true\n");
        assert!(cfg.sandbox_disabled);

        let _guard = ActiveConfigTestGuard::install(cfg);
        // Env absent/empty: the env parser contributes `None`, so only the knob can
        // opt out. Driven through the pure parser instead of the process env: the
        // env is process-global state that sibling tests read and write, and on
        // Windows `set_var(key, "")` cannot even express "present but empty" (it
        // deletes the key), so the env round-trip would test the wrong thing there.
        let env_opt_out = crate::harness::sandbox::opt_out_from_value(Some(""));
        assert_eq!(
            env_opt_out,
            crate::harness::pty::OptOut::None,
            "an empty env value is not an opt-out"
        );

        let opt_out = sandbox_opt_out(env_opt_out);
        assert_eq!(
            opt_out,
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{SANDBOX_DISABLED_KEY}=true")
            },
            "the typed knob must produce the opt-out that the harness feeds into SandboxInputs"
        );

        assert_eq!(
            sandbox_decision_for(opt_out),
            crate::harness::pty::SandboxDecision::Skip {
                resolved_exe: Some(std::path::PathBuf::from("/usr/local/bin/marmel-dev")),
                skip: crate::harness::pty::SandboxSkip::OptOut {
                    setting: format!("{SANDBOX_DISABLED_KEY}=true")
                }
            },
            "with the knob on, the harness must skip the Landlock re-entry (and warn)"
        );
    }

    /// (b) direction 1: knob OFF + env opt-out ⇒ the env opt-out still applies.
    #[test]
    fn sandbox_env_opt_out_wins_independently_of_the_knob() {
        let _guard = ActiveConfigTestGuard::install(Config {
            sandbox_disabled: false,
            ..Config::default()
        });
        // The env *value* is fed to the pure parser (`sandbox_opt_out` then reads the
        // config knob from the installed config). The env *round-trip* — the key
        // really being read from the process environment — is covered by
        // `sandbox_opt_out_env_round_trip_is_explicit_and_restored` below and by the
        // child-process tests in `src/harness/sandbox.rs`.
        let env_opt_out = crate::harness::sandbox::opt_out_from_value(Some("1"));
        assert_eq!(
            env_opt_out,
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{}=1", crate::harness::pty::SANDBOX_OPTOUT_ENV)
            },
            "the env fallback must keep working on its own"
        );

        let opt_out = sandbox_opt_out(env_opt_out);
        assert_eq!(
            opt_out,
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{}=1", crate::harness::pty::SANDBOX_OPTOUT_ENV)
            },
            "the env setting keeps attribution even with the knob present"
        );
        assert!(
            matches!(
                sandbox_decision_for(opt_out),
                crate::harness::pty::SandboxDecision::Skip {
                    skip: crate::harness::pty::SandboxSkip::OptOut { .. },
                    ..
                }
            ),
            "knob off + env set must still Skip"
        );
    }

    /// (b) direction 2: knob ON + an env value that is not an opt-out ⇒ Skip; with
    /// both set the outcome is the same Skip (never an AND of the two sources); with
    /// nothing opted out the sandbox stays on.
    ///
    /// The env side is asserted through the pure parser
    /// (`crate::harness::sandbox::opt_out_from_value`), the config side through the
    /// installed active config, so nothing in this test mutates process-global state
    /// that a sibling test could rewrite mid-assertion.
    #[test]
    fn sandbox_knob_opts_out_without_the_env_var() {
        let _guard = ActiveConfigTestGuard::install(Config {
            sandbox_disabled: true,
            ..Config::default()
        });

        // Present but falsy: not an env opt-out.
        let env_opt_out = crate::harness::sandbox::opt_out_from_value(Some("0"));
        assert_eq!(env_opt_out, crate::harness::pty::OptOut::None);

        let opt_out = sandbox_opt_out(env_opt_out.clone());
        assert_eq!(
            opt_out,
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{SANDBOX_DISABLED_KEY}=true")
            }
        );
        assert!(matches!(
            sandbox_decision_for(opt_out),
            crate::harness::pty::SandboxDecision::Skip {
                skip: crate::harness::pty::SandboxSkip::OptOut { .. },
                ..
            }
        ));

        // Both sources at once: still a Skip, attributed to the env variable.
        let both = sandbox_opt_out(crate::harness::sandbox::opt_out_from_value(Some("yes")));
        assert_eq!(
            both,
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{}=yes", crate::harness::pty::SANDBOX_OPTOUT_ENV)
            }
        );

        // And with the knob off and nothing in the env, the sandbox is ON. Expressed
        // through the pure merge (`false` = knob off) instead of installing a second
        // config inside the same test.
        assert_eq!(
            sandbox_decision_for(merge_sandbox_opt_out(
                crate::harness::sandbox::opt_out_from_value(None),
                false,
            )),
            crate::harness::pty::SandboxDecision::Apply {
                resolved_exe: std::path::PathBuf::from("/usr/local/bin/marmel-dev")
            },
            "nothing opted out ⇒ Landlock re-entry (fail-closed default)"
        );
    }

    /// The env half of the contract end to end: the key really is read from the
    /// process environment, and it is read fail-closed when it is absent.
    ///
    /// This is the **only** test in this binary that mutates
    /// `MARMEL_DISABLE_SANDBOX`. `SandboxEnvGuard` holds
    /// [`SANDBOX_ENV_TEST_SERIAL`] for the whole body (so the write cannot race with
    /// another test's read), sets the value explicitly (so the test never depends on
    /// an ambient value from the runner) and restores the previous state — including
    /// "unset" — on drop (so nothing leaks into the next test either, on any
    /// platform).
    #[test]
    fn sandbox_opt_out_env_round_trip_is_explicit_and_restored() {
        let set = SandboxEnvGuard::set("1");
        assert_eq!(
            crate::harness::sandbox::opt_out_from_env(),
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{}=1", crate::harness::pty::SANDBOX_OPTOUT_ENV)
            },
            "`MARMEL_DISABLE_SANDBOX=1` must reach the env reader"
        );
        assert_eq!(
            merge_sandbox_opt_out(crate::harness::sandbox::opt_out_from_env(), false),
            crate::harness::pty::OptOut::Explicit {
                setting: format!("{}=1", crate::harness::pty::SANDBOX_OPTOUT_ENV)
            },
            "an env opt-out keeps its attribution whatever the config knob says"
        );
        drop(set);

        let _cleared = SandboxEnvGuard::clear();
        assert_eq!(
            crate::harness::sandbox::opt_out_from_env(),
            crate::harness::pty::OptOut::None,
            "a cleared key must read as no env opt-out (fail-closed), never as a \
             value left behind by another test or by the runner"
        );
        assert_eq!(
            merge_sandbox_opt_out(crate::harness::sandbox::opt_out_from_env(), false),
            crate::harness::pty::OptOut::None,
            "neither source opted out ⇒ the sandbox stays on"
        );
    }

    /// (d) the configured `command_timeout_secs` is what the harness applies.
    #[test]
    fn command_timeout_secs_reaches_execution_timeout() {
        let cfg = partial_from("command_timeout_secs = 17\n");
        assert_eq!(cfg.command_timeout_secs, 17, "parsed and merged");

        let _guard = ActiveConfigTestGuard::install(cfg);
        let applied = effective_command_timeout_secs(None);
        assert_eq!(applied, 17, "the configured value must be the applied one");
        assert_eq!(
            std::time::Duration::from_secs(applied),
            std::time::Duration::from_secs(17),
            "the value handed to the execution timeout is the configured one"
        );

        // A per-call override still wins over the configured default.
        assert_eq!(effective_command_timeout_secs(Some(42)), 42);
    }

    /// The default config keeps the documented built-in timeout. A separate test:
    /// installing a second active config inside the test above would take the
    /// process-global test lock a second time on one thread.
    #[test]
    fn default_config_keeps_the_builtin_command_timeout() {
        let _guard = ActiveConfigTestGuard::install(Config::default());
        assert_eq!(
            active_command_timeout_secs(),
            DEFAULT_COMMAND_TIMEOUT_SECS,
            "the default config keeps 60 s"
        );
    }

    #[test]
    fn command_timeout_secs_default_when_no_config_is_active() {
        // "No active config" is process-global state: serialized with the other
        // tests that read or write `ACTIVE_CONFIG`.
        let _serial = sandbox_env_test_lock();
        let previous = get_active();
        clear_active();
        let applied = effective_command_timeout_secs(None);
        match previous {
            Some(cfg) => set_active(cfg),
            None => clear_active(),
        }
        assert_eq!(applied, DEFAULT_COMMAND_TIMEOUT_SECS);
        assert_eq!(Config::default().command_timeout_secs, 60);
    }

    /// Out-of-range values (from the file or from the model) are clamped into the
    /// documented range instead of being applied verbatim.
    #[test]
    fn command_timeout_secs_out_of_range_is_clamped() {
        for (configured, expected) in [
            (0, MIN_COMMAND_TIMEOUT_SECS),
            (9999, MAX_COMMAND_TIMEOUT_SECS),
            (30, 30),
        ] {
            let _guard = ActiveConfigTestGuard::install(Config {
                command_timeout_secs: configured,
                ..Config::default()
            });
            let applied = effective_command_timeout_secs(None);
            assert_eq!(
                applied, expected,
                "`command_timeout_secs = {configured}` must apply as {expected} s"
            );
            assert!(
                (MIN_COMMAND_TIMEOUT_SECS..=MAX_COMMAND_TIMEOUT_SECS).contains(&applied),
                "applied value left the documented range: {applied}"
            );
        }

        let _guard = ActiveConfigTestGuard::install(Config::default());
        assert_eq!(
            effective_command_timeout_secs(Some(0)),
            MIN_COMMAND_TIMEOUT_SECS
        );
        assert_eq!(
            effective_command_timeout_secs(Some(9999)),
            MAX_COMMAND_TIMEOUT_SECS
        );
    }

    /// The harness call site really reads both knobs: a renamed or removed
    /// wiring line must fail here, not in production.
    #[test]
    fn harness_call_site_wires_both_config_knobs() {
        let src =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/harness/pty.rs"))
                .expect("src/harness/pty.rs must be readable");

        assert!(
            src.contains("crate::config::sandbox_opt_out(opt_out_from_env())"),
            "the SandboxInputs construction must feed the config knob into \
             SandboxInputs::opt_out alongside the env fallback"
        );
        assert!(
            src.contains("crate::config::effective_command_timeout_secs("),
            "run_command must take its timeout from the config, not the built-in constant"
        );
        assert!(
            src.contains("opt_out_from_env()"),
            "the env-only opt-out must keep working on its own"
        );
    }

    // ------------------------------------------------------------ session keys
    //
    // `PartialConfig` has no `deny_unknown_fields`, so a `session_*` key that is
    // missing from the mirror — or from `merge` — is dropped **silently** by
    // `toml::from_str`. These tests are the only mechanism that catches that
    // drift (flow §3.1 T-CFG-1/T-CFG-2; the existing `max_fix_loop_rounds` key is
    // the in-repo proof that it happens).

    fn merged(toml_str: &str) -> Config {
        let partial: PartialConfig = toml::from_str(toml_str).expect("parses");
        merge(Config::default(), partial)
    }

    #[test]
    fn session_key_defaults() {
        let cfg = Config::default();
        assert!(cfg.session_enabled);
        assert!(!cfg.session_resume);
        assert_eq!(cfg.session_commit_min_interval_ms, 250);
        assert_eq!(cfg.session_worker_mirror_max_messages, 4000);
        assert!(cfg.session_commit_on_tool_result);
        assert_eq!(cfg.session_max_bundle_bytes, 32 * 1024 * 1024);
    }

    #[test]
    fn session_key_session_enabled_merges() {
        let cfg = merged("session_enabled = false\n");
        assert!(!cfg.session_enabled);
        assert!(
            merged("").session_enabled,
            "an absent key keeps the default"
        );
    }

    #[test]
    fn session_key_session_resume_merges() {
        assert!(merged("session_resume = true\n").session_resume);
    }

    #[test]
    fn session_key_session_commit_min_interval_ms_merges() {
        assert_eq!(
            merged("session_commit_min_interval_ms = 0\n").session_commit_min_interval_ms,
            0
        );
        assert_eq!(
            merged("session_commit_min_interval_ms = 900\n").session_commit_min_interval_ms,
            900
        );
    }

    #[test]
    fn session_key_session_worker_mirror_max_messages_merges() {
        assert_eq!(
            merged("session_worker_mirror_max_messages = 25\n").session_worker_mirror_max_messages,
            25
        );
    }

    #[test]
    fn session_key_session_commit_on_tool_result_merges() {
        assert!(!merged("session_commit_on_tool_result = false\n").session_commit_on_tool_result);
    }

    #[test]
    fn session_key_session_max_bundle_bytes_merges() {
        assert_eq!(
            merged("session_max_bundle_bytes = 1024\n").session_max_bundle_bytes,
            1024
        );
    }

    #[test]
    fn all_six_session_keys_merge_together() {
        let cfg = merged(
            r#"
session_enabled = false
session_resume = true
session_commit_min_interval_ms = 500
session_worker_mirror_max_messages = 10
session_commit_on_tool_result = false
session_max_bundle_bytes = 4096
"#,
        );
        assert!(!cfg.session_enabled);
        assert!(cfg.session_resume);
        assert_eq!(cfg.session_commit_min_interval_ms, 500);
        assert_eq!(cfg.session_worker_mirror_max_messages, 10);
        assert!(!cfg.session_commit_on_tool_result);
        assert_eq!(cfg.session_max_bundle_bytes, 4096);
    }

    /// T-CFG-2: every key must exist in all four places, or it is unsettable.
    #[test]
    fn every_session_key_exists_in_config_partial_merge_and_the_example() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/config.rs"))
            .expect("src/config.rs must be readable");
        let example =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/marmel.toml.example"))
                .expect("marmel.toml.example must be readable");

        let slice = |start: &str, end: &str| -> String {
            let from = src
                .find(start)
                .unwrap_or_else(|| panic!("{start} must exist in src/config.rs"));
            let rest = &src[from..];
            let to = rest.find(end).unwrap_or(rest.len());
            rest[..to].to_string()
        };
        let config_struct = slice("pub struct Config {", "impl Default for Config {");
        let defaults = slice("impl Default for Config {", "static ACTIVE_CONFIG");
        let partial_struct = slice("struct PartialConfig {", "struct PartialMonitoringConfig {");
        let merge_body = slice("fn merge(mut base: Config", "impl Config {");

        const KEYS: [&str; 6] = [
            "session_enabled",
            "session_resume",
            "session_commit_min_interval_ms",
            "session_worker_mirror_max_messages",
            "session_commit_on_tool_result",
            "session_max_bundle_bytes",
        ];
        for key in KEYS {
            assert!(
                config_struct.contains(&format!("pub {key}:")),
                "Config must declare {key}"
            );
            assert!(
                defaults.contains(key),
                "impl Default for Config must set {key}"
            );
            assert!(
                partial_struct.contains(&format!("pub {key}: Option<")),
                "PartialConfig must mirror {key} as Option, otherwise the key is \
                 silently dropped by toml"
            );
            assert!(
                merge_body.contains(&format!("partial.{key}"))
                    && merge_body.contains(&format!("base.{key} =")),
                "merge must apply {key}"
            );
            assert!(
                example.contains(&format!("# {key} =")),
                "marmel.toml.example must document {key}"
            );
        }
    }

    #[test]
    fn env_flag_reads_the_documented_truthy_and_falsy_words_only() {
        for truthy in ["1", "true", "TRUE", " yes ", "on"] {
            assert_eq!(env_flag(Some(&truthy.to_string())), Some(true), "{truthy}");
        }
        for falsy in ["0", "false", "FALSE", " off ", "no"] {
            assert_eq!(env_flag(Some(&falsy.to_string())), Some(false), "{falsy}");
        }
        assert_eq!(env_flag(None), None);
        assert_eq!(env_flag(Some(&String::new())), None);
        assert_eq!(env_flag(Some(&"maybe".to_string())), None);
    }

    #[test]
    fn resume_env_overrides_the_file_value_both_ways() {
        let file = merged("session_resume = true\n");
        assert!(
            !apply_session_env(file, Some("0".to_string()), None, None).session_resume,
            "an explicit falsy MARMEL_RESUME means a fresh run"
        );
        let file = Config::default();
        assert!(apply_session_env(file, Some("1".to_string()), None, None).session_resume);
    }

    #[test]
    fn an_unrecognized_resume_value_keeps_the_file_value() {
        let file = merged("session_resume = true\n");
        let cfg = apply_session_env(file, Some("sure!".to_string()), None, None);
        assert!(cfg.session_resume, "junk must not silently flip the key");
    }

    #[test]
    fn session_disable_env_is_a_one_way_switch() {
        let on = Config::default();
        assert!(
            !apply_session_env(on, None, Some("1".to_string()), None).session_enabled,
            "MARMEL_SESSION_DISABLE must turn capture off"
        );
        let off = merged("session_enabled = false\n");
        assert!(
            !apply_session_env(off.clone(), None, Some("0".to_string()), None).session_enabled,
            "MARMEL_SESSION_DISABLE must never turn capture back on"
        );
        let after = apply_session_env(off, None, Some("nonsense".to_string()), None);
        assert!(
            !after.session_enabled,
            "junk must not change capture state either"
        );
    }

    #[test]
    fn commit_floor_env_beats_the_file_and_junk_is_ignored() {
        let file = merged("session_commit_min_interval_ms = 700\n");
        assert_eq!(
            apply_session_env(file.clone(), None, None, Some("25".to_string()))
                .session_commit_min_interval_ms,
            25
        );
        assert_eq!(
            apply_session_env(file.clone(), None, None, Some("fast".to_string()))
                .session_commit_min_interval_ms,
            700,
            "a non-integer floor must not become 0"
        );
        assert_eq!(
            apply_session_env(file, None, None, None).session_commit_min_interval_ms,
            700
        );
    }
}
