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
/// (`session_commit_min_interval_ms`).
pub const DEFAULT_SESSION_COMMIT_MIN_INTERVAL_MS: u64 = 250;

/// Default cap on the in-memory per-worker mirror
/// (`session_worker_mirror_max_messages`).
pub const DEFAULT_SESSION_WORKER_MIRROR_MAX_MESSAGES: usize = 4000;

/// Default soft ceiling on total session-bundle member bytes
/// (`session_max_bundle_bytes`).
pub const DEFAULT_SESSION_MAX_BUNDLE_BYTES: u64 = 32 * 1024 * 1024;

/// Environment key that requests a session resume for one run
/// (`1`/`true` ⇒ true). Equivalent to `--resume`.
pub const SESSION_RESUME_ENV: &str = "MARMEL_RESUME";

/// Environment key that disables session capture. **One-way switch**: it can only
/// turn capture off, never on, mirroring how `MARMEL_DISABLE_SANDBOX` ORs with
/// [`Config::sandbox_disabled`].
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
    /// Simultaneous-session caps (`[orchestration.concurrency]`). Unset ⇒
    /// **unlimited**, i.e. exactly today's behaviour.
    pub concurrency: ConcurrencyConfig,
}

impl OrchestrationConfig {
    pub fn default_depth() -> Self {
        Self {
            max_recursion_depth: DEFAULT_MAX_RECURSION_DEPTH,
            ..Self::default()
        }
    }
}

/// Concurrency limits parsed from `[orchestration.concurrency]`.
///
/// `max_sessions` caps how many LLM sessions may be in flight at once for one
/// `(backend_url, model)` pair. `None` (the default) means **unlimited**: nothing
/// is metered and behaviour is exactly what it is today. `by_backend` holds
/// per-URL overrides keyed by `backend_url`, and both live in the same TOML
/// table:
///
/// ```toml
/// [orchestration.concurrency]
/// max_sessions = 4                            # default cap for every pair
/// "http://192.168.50.162:8080/v1" = 2         # per-backend override
/// ```
///
/// Because the overrides share the table with `max_sessions`, they are collected
/// through `#[serde(flatten)]` — a plain named `backends` field would silently
/// drop them (verified: `backends: {}` while the table clearly contains them).
/// Three consequences of `flatten` are pinned by tests below:
///
/// - a URL key **must be quoted**; a bare `http://host/v1 = 2` is a TOML syntax
///   error, never a dropped key;
/// - any value that is not an integer is a parse error, not a silent default;
/// - a **typo'd** key (`max_session = 4`) is not ignored — it becomes an inert
///   extra map entry that can never match a backend.
///   [`ConcurrencyConfig::suspect_backend_keys`] is the warn-only guard for that
///   last case.
///
/// Keys are never compared verbatim: both sides of the lookup go through
/// [`backend_key`], so trailing slashes and stray whitespace are ignored.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct ConcurrencyConfig {
    /// Per-backend caps keyed by `backend_url`, flattened into the same table as
    /// `max_sessions` — hence the quoted URL keys. Matched through
    /// [`backend_key`], never by raw string equality.
    #[serde(flatten)]
    pub by_backend: HashMap<String, usize>,
    /// Default cap for every backend/model pair. `None` ⇒ unlimited.
    ///
    /// A value below [`MIN_MAX_SESSIONS`] (i.e. `0`) is never applied: it would
    /// deadlock a `Semaphore::new(0)`. It fails **open** to unlimited with an
    /// attributable warning — see [`resolve_session_cap`]. It is deliberately
    /// *not* clamped up to `1`, which would silently serialize everything.
    pub max_sessions: Option<usize>,
}

impl ConcurrencyConfig {
    /// `true` when nothing is configured at all: no default cap and no
    /// per-URL override, i.e. today's unlimited behaviour.
    pub fn is_unlimited(&self) -> bool {
        self.max_sessions.is_none() && self.by_backend.is_empty()
    }

    /// The override table re-keyed through [`backend_key`], which is what makes
    /// a config key and a runtime `backend_url` reduce to the same string.
    ///
    /// Deterministic even when two spellings collapse onto one key
    /// (`http://host/v1` and `http://host/v1/`): raw keys are visited in sorted
    /// order and the first one wins.
    pub fn normalized_backends(&self) -> BTreeMap<String, usize> {
        let mut entries: Vec<(String, usize)> = self
            .by_backend
            .iter()
            .map(|(key, value)| (key.clone(), *value))
            .collect();
        entries.sort();
        let mut normalized = BTreeMap::new();
        for (key, value) in entries {
            normalized.entry(backend_key(&key)).or_insert(value);
        }
        normalized
    }

    /// Override keys that do not look like backend URLs.
    ///
    /// `#[serde(flatten)]` cannot tell `max_session = 4` (a typo of
    /// `max_sessions`) from a per-URL override, so the typo lands here instead of
    /// being ignored. Such an entry is inert — it can never match a backend — so
    /// this is reported as a warning, never as a parse failure. Sorted for a
    /// stable diagnostic.
    pub fn suspect_backend_keys(&self) -> Vec<String> {
        let mut suspects: Vec<String> = self
            .by_backend
            .keys()
            .filter(|key| {
                let key = key.trim();
                !(key.starts_with("http://") || key.starts_with("https://"))
            })
            .cloned()
            .collect();
        suspects.sort();
        suspects
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
    /// Optional reasoning-effort override for this specialist's validator
    /// (free-form pass-through; backends use different vocabularies).
    pub validator_reasoning_effort: Option<String>,
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
    /// Optional reasoning-effort override for this specialist (free-form
    /// pass-through; backends use different vocabularies). A dial, not a cap:
    /// it is independent of [`Self::max_thinking_tokens`], which stays the
    /// ceiling. Unset ⇒ the field is not sent.
    pub reasoning_effort: Option<String>,
    /// MCP servers whose tools this specialist is allowed to see.
    pub mcp_servers: Vec<String>,
    /// Optional custom prompt / instructions appended to this specialist's system prompt.
    #[serde(alias = "instructions", alias = "system_prompt", alias = "base_prompt")]
    pub prompt: Option<String>,
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
    /// Extra paths the Landlock sandbox must allow **read + execute** for, on top
    /// of the built-in allow-list (`crate::harness::sandbox`).
    ///
    /// Empty by default: the built-in list is used as-is. The key exists for hosts
    /// whose executables do not live under the FHS prefixes that list covers —
    /// NixOS (`/nix/store`) and Guix (`/gnu/store`), where `/bin` and friends are
    /// symlink farms and every program resolves below the store — and for a
    /// non-default `store = ...` prefix in `nix.conf`.
    ///
    /// Merge semantics follow the only other array-of-strings key,
    /// `OrchestrationConfig::mcp_servers`: a **non-empty** file value replaces this
    /// field outright (it never appends to an existing value), while an absent key
    /// — or an explicit `[]`, which serde cannot tell apart — keeps the default.
    /// Consumed by the sandbox ruleset, not by this module.
    pub sandbox_extra_readonly_paths: Vec<String>,
    pub max_repetition_threshold: usize,
    pub enable_xml_rescue: bool,
    pub ui_mode: String,
    /// Detailed debug logging to debug.log.
    pub debug: bool,
    /// Maximum reasoning/thinking tokens per single turn before cutting off runaway reasoning.
    pub max_thinking_tokens: usize,
    /// Optional reasoning-effort hint (`none`/`minimal`/`low`/`medium`/`high` are the
    /// documented vocabulary, but the value is free-form pass-through — backends use
    /// different vocabularies and this layer performs no vocabulary check). Applies to
    /// the Manager and to roles that do not override it; `SpecialistConfig::reasoning_effort`
    /// and `SpecialistConfig::validator_reasoning_effort` win per role. Unset ⇒ the field
    /// is omitted from the request.
    pub reasoning_effort: Option<String>,
    /// Resilience harness thresholds.
    pub monitoring: Option<MonitoringConfig>,
    pub orchestration: OrchestrationConfig,
    /// Configured external MCP servers (`[mcp_servers.<name>]`).
    pub mcp_servers: HashMap<String, crate::mcp::McpServerConfig>,
    /// Optional base prompt appended to the orchestrator system prompt.
    pub base_prompt: Option<String>,
    /// Optional global cap on fix-loop rounds per validation pass (defaults to MAX_FIX_LOOP_ROUNDS = 200).
    pub max_fix_loop_rounds: Option<usize>,
    /// Session capture/rehydration master switch. Default `true`: a bundle is
    /// always written.
    ///
    /// One-way against the environment: `MARMEL_SESSION_DISABLE` can turn capture
    /// **off**, never on — see [`session_env_overrides`].
    pub session_enabled: bool,
    /// Rehydrate from the bundle at boot. Default `false`: a fresh start is the
    /// default behaviour; `--resume` / `MARMEL_RESUME` turn it on for one run.
    pub session_resume: bool,
    /// Floor between throttled commits. Default 250 ms.
    pub session_commit_min_interval_ms: u64,
    /// Cap on the in-memory per-worker mirror. Default 4000 records.
    pub session_worker_mirror_max_messages: usize,
    /// Commit at every landed tool result. `false` drops the tool-result commits:
    /// fewer writes, coarser crash granularity. Default `true`.
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
            sandbox_extra_readonly_paths: Vec::new(),
            max_repetition_threshold: 5,
            enable_xml_rescue: true,
            ui_mode: "tui".to_string(),
            debug: false,
            max_thinking_tokens: DEFAULT_MAX_THINKING_TOKENS,
            reasoning_effort: None,
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

/// Config key that carries the default simultaneous-session cap, used verbatim
/// in the attributable warning that [`resolve_session_cap`] logs.
pub const MAX_SESSIONS_KEY: &str = "orchestration.concurrency.max_sessions";

/// Config table that holds the concurrency keys, used verbatim in the
/// attributable labels that [`resolve_session_cap`] and
/// [`ConcurrencyConfig::suspect_backend_keys`] diagnostics produce.
pub const CONCURRENCY_TABLE: &str = "orchestration.concurrency";

/// Inclusive lower bound of a usable session cap.
///
/// A configured cap below this bound is never applied: `Semaphore::new(0)` can
/// never be acquired, so a `0` would wedge every session against that backend.
/// The fail-open rule (unlimited, plus a warning) is in [`resolve_session_cap`];
/// it is deliberately not a clamp to `1`, which would silently serialize a
/// multi-backend configuration the user asked to run in parallel.
pub const MIN_MAX_SESSIONS: usize = 1;

/// Normalize a backend URL into the key used on **both** sides of a concurrency
/// lookup: a `[orchestration.concurrency]` override key and a runtime
/// `backend_url` must reduce to the same string, or the override silently misses.
///
/// Same rule the wire path already applies (`ChatClient` trims trailing slashes
/// before appending `/chat/completions`), so `http://host/v1/` and
/// `http://host/v1` are one backend here exactly as they are one backend on the
/// wire. Runtime callers (the semaphore pool and the client that acquires from
/// it) must key off this helper rather than off a raw URL.
pub fn backend_key(url: &str) -> String {
    url.trim().trim_end_matches('/').to_string()
}

/// What `[orchestration.concurrency]` resolves to for one backend URL.
///
/// Carries the attribution alongside the number so no consumer has to
/// reconstruct which key produced the cap — the pool's queue-timeout error names
/// the cap and the key it belongs to, and this is where both come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionCap {
    /// Cap to apply. `None` ⇒ **unlimited**: nothing is metered, today's
    /// behaviour.
    pub limit: Option<usize>,
    /// Key the value came from (`orchestration.concurrency.max_sessions`, or
    /// `orchestration.concurrency."http://host:8080/v1"` for an override).
    /// `None` when no key matched.
    pub source: Option<String>,
    /// Set when a configured value was **rejected**: what was configured, under
    /// which key, and what is applied instead. Reported by
    /// [`resolve_max_sessions`]; the pure seam only returns it.
    pub rejected: Option<String>,
}

/// Pure precedence + validation seam behind the concurrency cap:
/// per-URL override → `max_sessions` → unlimited.
///
/// Both sides of the override lookup go through [`backend_key`], so a config key
/// written `http://host:8080/v1/` (or with stray spaces) still matches a runtime
/// `http://host:8080/v1`. A cap below [`MIN_MAX_SESSIONS`] is reported in
/// `SessionCap::rejected` and dropped: the resolved limit is `None` (unlimited),
/// never `Some(0)` and never clamped to `Some(1)`. This function performs no
/// logging and touches no global state, so the precedence, the normalization and
/// the zero rule are all assertable without a process-global config.
pub fn resolve_session_cap(concurrency: &ConcurrencyConfig, backend_url: &str) -> SessionCap {
    let key = backend_key(backend_url);
    let (raw, source) = match concurrency.normalized_backends().get(&key) {
        Some(value) => (Some(*value), Some(format!("{CONCURRENCY_TABLE}.{key:?}"))),
        None => match concurrency.max_sessions {
            Some(value) => (Some(value), Some(MAX_SESSIONS_KEY.to_string())),
            None => (None, None),
        },
    };

    match raw {
        Some(value) if value < MIN_MAX_SESSIONS => SessionCap {
            limit: None,
            source: source.clone(),
            rejected: Some(format!(
                "{} = {value} is below the minimum usable cap of {MIN_MAX_SESSIONS}",
                source.unwrap_or_else(|| MAX_SESSIONS_KEY.to_string()),
            )),
        },
        value => SessionCap {
            limit: value,
            source,
            rejected: None,
        },
    }
}

/// The cap applied to the pair keyed by `backend_url`, from a caller-supplied
/// table: per-URL override, else `max_sessions`, else unlimited.
///
/// A rejected value ([`MIN_MAX_SESSIONS`] violation) is logged — naming the key,
/// the offending value and what is applied instead — and resolves to unlimited.
pub fn resolve_max_sessions(concurrency: &ConcurrencyConfig, backend_url: &str) -> Option<usize> {
    let cap = resolve_session_cap(concurrency, backend_url);
    if let Some(rejected) = cap.rejected.as_deref() {
        tracing::warn!(
            key = cap.source.as_deref().unwrap_or(MAX_SESSIONS_KEY),
            backend = %backend_url,
            valid_range = format!("{MIN_MAX_SESSIONS}..="),
            "configured concurrency cap is unusable; failing open to unlimited: {rejected}"
        );
    }
    cap.limit
}

/// The cap for the pair keyed by `backend_url` according to the globally active
/// config; `None` when no config is active, or when nothing is configured —
/// both mean **unlimited**, i.e. behaviour unchanged from before this key
/// existed.
pub fn effective_max_sessions(backend_url: &str) -> Option<usize> {
    match get_active() {
        Some(cfg) => resolve_max_sessions(&cfg.orchestration.concurrency, backend_url),
        None => None,
    }
}

/// Warn (never fail) about `[orchestration.concurrency]` override keys that do
/// not look like backend URLs.
///
/// `#[serde(flatten)]` cannot distinguish a typo such as `max_session = 4` from a
/// per-URL override, so the typo becomes an inert map entry that silently does
/// nothing. The table is only ever read from one file, so this runs once per
/// load.
fn warn_suspect_backend_keys(concurrency: &ConcurrencyConfig) {
    for key in concurrency.suspect_backend_keys() {
        tracing::warn!(
            key = %format!("{CONCURRENCY_TABLE}.{key:?}"),
            "not a backend URL: a typo in this table becomes an override key that can \
             never match a backend, so it does nothing"
        );
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
        warn_suspect_backend_keys(&cfg.orchestration.concurrency);
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

    // Session keys: env beats file beats default, with the deliberate
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

/// Applies the three session environment overrides to `cfg`.
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
    /// Array-of-strings mirror using the `#[serde(default)]` + plain `Vec` idiom of
    /// `PartialOrchestrationConfig::mcp_servers`: an absent key yields an empty
    /// `Vec` (never a parse error) and `merge` reads that empty `Vec` as "unset",
    /// so the default survives.
    #[serde(default)]
    pub sandbox_extra_readonly_paths: Vec<String>,
    pub max_repetition_threshold: Option<usize>,
    pub enable_xml_rescue: Option<bool>,
    pub max_thinking_tokens: Option<usize>,
    pub reasoning_effort: Option<String>,
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
    /// `Option` (not a plain mirror) so "block absent" stays distinguishable
    /// from "block present but empty"; both mean unlimited, and the
    /// `monitoring: Option<PartialMonitoringConfig>` precedent is why.
    pub concurrency: Option<PartialConcurrencyConfig>,
}

/// Mandatory mirror of [`ConcurrencyConfig`] (see the "four places" rule: a key
/// missing from the mirror or from `merge` is dropped **silently** by `toml`).
///
/// The `flatten` is load-bearing: without it the quoted URL keys sitting beside
/// `max_sessions` in `[orchestration.concurrency]` are the verified silent-drop
/// case. Keys are kept exactly as written here and normalized only at read time
/// through [`backend_key`] — `MARMEL_BACKEND_URL` can change the runtime URL
/// after this merge, and merge is a pure field applier.
#[derive(Debug, Clone, Deserialize, Default)]
struct PartialConcurrencyConfig {
    #[serde(flatten)]
    pub by_backend: HashMap<String, usize>,
    pub max_sessions: Option<usize>,
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
    // List merge mirrors `orchestration.mcp_servers` below (the only other
    // array-of-strings key): **replace when non-empty, never append**. An absent
    // key and an explicit `[]` both deserialize to an empty `Vec` and therefore
    // keep the default — a file can set this list but cannot clear it.
    if !partial.sandbox_extra_readonly_paths.is_empty() {
        base.sandbox_extra_readonly_paths = partial.sandbox_extra_readonly_paths;
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
    if let Some(v) = partial.reasoning_effort {
        base.reasoning_effort = Some(v);
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
        if let Some(p_con) = p_orch.concurrency {
            if let Some(m) = p_con.max_sessions {
                base.orchestration.concurrency.max_sessions = Some(m);
            }
            // Map merge follows `specialists` (extend), not `mcp_servers`
            // (replace): only one file is ever read, so this is default-vs-file.
            if !p_con.by_backend.is_empty() {
                base.orchestration
                    .concurrency
                    .by_backend
                    .extend(p_con.by_backend);
            }
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
    // `clippy::unnecessary_lazy_evaluations` on the windows target only, and a
    // cross-target `-D warnings` build must stay green. Semantics are unchanged:
    // `HOME` first, platform fallback second.
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
/// (`.github/workflows/ci.yml`). This lock, not the thread count, is what makes
/// those writes sound.
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
        // Concurrency defaults to no cap at all: `None`, not `Some(0)` and not
        // `Some(1)` — unset must mean today's unlimited behaviour.
        assert_eq!(cfg.orchestration.concurrency.max_sessions, None);
        assert!(cfg.orchestration.concurrency.by_backend.is_empty());
        assert!(cfg.orchestration.concurrency.is_unlimited());
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
        assert!(
            cfg.orchestration.concurrency.is_unlimited(),
            "an absent `[orchestration.concurrency]` must change nothing"
        );
    }

    // ------------------------------------------------- [orchestration.concurrency]
    //
    // The shape is the one `feature.md` specifies: `max_sessions` plus quoted URL
    // keys **in the same table**. That is only reachable through
    // `#[serde(flatten)]` (a plain named map field silently drops the URL keys —
    // verified in docs/research_concurrency_config_delta.md §2), so these tests
    // are the regression guard for both the parse and the resolution rules.

    /// `[orchestration.concurrency]` present but empty is a valid no-op: no
    /// default cap, no overrides, unlimited.
    #[test]
    fn concurrency_block_present_but_empty_is_a_no_op() {
        let cfg = merged("[orchestration]\n[orchestration.concurrency]\n");
        assert_eq!(cfg.orchestration.concurrency.max_sessions, None);
        assert!(cfg.orchestration.concurrency.by_backend.is_empty());
        assert!(cfg.orchestration.concurrency.is_unlimited());
    }

    #[test]
    fn concurrency_max_sessions_parses_into_the_block() {
        let cfg = merged("[orchestration.concurrency]\nmax_sessions = 4\n");
        assert_eq!(cfg.orchestration.concurrency.max_sessions, Some(4));
        assert!(
            cfg.orchestration.concurrency.by_backend.is_empty(),
            "`max_sessions` must not leak into the override table"
        );
        assert_eq!(
            resolve_max_sessions(&cfg.orchestration.concurrency, "http://localhost:8000/v1"),
            Some(4),
            "the default cap applies to any backend when no override matches"
        );
    }

    /// The verbatim `feature.md` block: `max_sessions` **and** quoted URL keys in
    /// one table. A plain named map field would leave `by_backend` empty here,
    /// which is why the URL keys are asserted one by one.
    #[test]
    fn concurrency_url_keys_parse_beside_max_sessions() {
        let cfg = merged(
            r#"
[orchestration.concurrency]
max_sessions = 4
"http://192.168.50.162:8080/v1" = 2
"https://us.example.com/v1" = 8
"#,
        );
        let con = &cfg.orchestration.concurrency;
        assert_eq!(con.max_sessions, Some(4));
        assert_eq!(
            con.by_backend.len(),
            2,
            "the URL keys must land in the override table, not be silently dropped"
        );
        assert_eq!(
            con.by_backend.get("http://192.168.50.162:8080/v1"),
            Some(&2)
        );
        assert_eq!(con.by_backend.get("https://us.example.com/v1"), Some(&8));
        assert_eq!(
            con.normalized_backends()
                .get("http://192.168.50.162:8080/v1"),
            Some(&2),
            "a well-formed key survives normalization unchanged"
        );
    }

    /// Precedence: the per-URL override wins for its own backend, every other
    /// backend keeps `max_sessions`, and an unset table caps nothing.
    #[test]
    fn concurrency_override_wins_per_backend_and_the_default_covers_the_rest() {
        let cfg = merged(
            r#"
[orchestration.concurrency]
max_sessions = 4
"http://192.168.50.162:8080/v1" = 2
"#,
        );
        let con = &cfg.orchestration.concurrency;
        assert_eq!(
            resolve_max_sessions(con, "http://192.168.50.162:8080/v1"),
            Some(2),
            "the per-URL override wins over the default cap"
        );
        assert_eq!(
            resolve_max_sessions(con, "http://localhost:8000/v1"),
            Some(4),
            "an unmatched backend keeps the default cap"
        );
        assert_eq!(
            resolve_max_sessions(&Config::default().orchestration.concurrency, "http://x/v1"),
            None,
            "unset means unlimited"
        );
    }

    /// Both sides of the lookup go through `backend_key`, so a trailing slash or
    /// stray whitespace on **either** side cannot make an override silently miss.
    #[test]
    fn backend_key_normalizes_both_the_config_key_and_the_runtime_url() {
        assert_eq!(
            backend_key("http://192.168.50.162:8080/v1/"),
            "http://192.168.50.162:8080/v1"
        );
        assert_eq!(
            backend_key("  http://192.168.50.162:8080/v1//  "),
            "http://192.168.50.162:8080/v1"
        );
        assert_eq!(
            backend_key("http://localhost:8000"),
            "http://localhost:8000"
        );
        assert_eq!(backend_key(""), "");
        assert_eq!(
            backend_key("http://localhost:8000/v1?x=1/"),
            "http://localhost:8000/v1?x=1",
            "only the trailing slash is stripped; the path itself is opaque"
        );

        let slash_key = merged(
            "[orchestration.concurrency]\n\"http://192.168.50.162:8080/v1/  \" = 2\nmax_sessions = 4\n",
        );
        assert_eq!(
            resolve_max_sessions(
                &slash_key.orchestration.concurrency,
                "http://192.168.50.162:8080/v1"
            ),
            Some(2),
            "a trailing slash on the config key must still match a clean runtime URL"
        );

        let clean_key =
            merged("[orchestration.concurrency]\n\"http://192.168.50.162:8080/v1\" = 2\n");
        assert_eq!(
            resolve_max_sessions(
                &clean_key.orchestration.concurrency,
                " http://192.168.50.162:8080/v1/// "
            ),
            Some(2),
            "a trailing slash on the runtime URL must still match a clean config key"
        );
    }

    /// Two spellings of one backend collapse onto one key; the rule that decides
    /// which value wins is pinned so the resolution can never depend on `HashMap`
    /// iteration order.
    #[test]
    fn collapsing_url_keys_resolve_deterministically() {
        let cfg = merged(
            "[orchestration.concurrency]\n\"http://h/v1/\" = 9\n\"http://h/v1\" = 2\nmax_sessions = 5\n",
        );
        let con = &cfg.orchestration.concurrency;
        assert_eq!(con.by_backend.len(), 2, "raw keys are kept as written");
        assert_eq!(
            con.normalized_backends(),
            [("http://h/v1".to_string(), 2)].into_iter().collect(),
            "sorted raw keys, first write wins"
        );
        assert_eq!(resolve_max_sessions(con, "http://h/v1"), Some(2));
        assert_eq!(resolve_max_sessions(con, "http://h/v1/"), Some(2));
    }

    /// `max_sessions = 0` must fail **open** to unlimited with an attributable
    /// warning — never clamp to 1, which would silently serialize everything.
    #[test]
    fn concurrency_zero_fails_open_to_unlimited_and_names_the_key() {
        let cfg = merged("[orchestration.concurrency]\nmax_sessions = 0\n");
        let con = &cfg.orchestration.concurrency;
        // Parsing keeps the configured value verbatim; the read seam is where it
        // is rejected (so the warning can name what the file actually said).
        assert_eq!(con.max_sessions, Some(0));

        let cap = resolve_session_cap(con, "http://localhost:8000/v1");
        assert_eq!(
            cap.limit, None,
            "a 0 cap must resolve to unlimited, not to Some(1)"
        );
        assert_eq!(cap.source.as_deref(), Some(MAX_SESSIONS_KEY));
        let rejected = cap
            .rejected
            .expect("a rejected cap must carry the attribution the warning logs");
        assert!(
            rejected.contains(MAX_SESSIONS_KEY),
            "the rejection must name the offending key, got {rejected:?}"
        );
        assert!(
            rejected.contains("= 0") && rejected.contains(&MIN_MAX_SESSIONS.to_string()),
            "the rejection must name the configured value and the bound it broke, got {rejected:?}"
        );
        assert_eq!(resolve_max_sessions(con, "http://localhost:8000/v1"), None);
    }

    /// A zero override is an explicit statement about that one backend: it fails
    /// open for **that** backend only, and does not fall back to `max_sessions`
    /// (an override that matched is never second-guessed by the looser default).
    #[test]
    fn concurrency_zero_override_uncaps_only_the_backend_it_names() {
        let cfg = merged(
            "[orchestration.concurrency]\nmax_sessions = 4\n\"http://192.168.50.162:8080/v1\" = 0\n",
        );
        let con = &cfg.orchestration.concurrency;
        let cap = resolve_session_cap(con, "http://192.168.50.162:8080/v1/");
        assert_eq!(cap.limit, None);
        assert_eq!(
            cap.source.as_deref(),
            Some("orchestration.concurrency.\"http://192.168.50.162:8080/v1\""),
            "attribution points at the override, not at `max_sessions`"
        );
        assert!(
            cap.rejected
                .as_deref()
                .is_some_and(|r| r.contains("http://192.168.50.162:8080/v1")),
            "the rejection must name the URL key that was set to 0"
        );
        assert_eq!(
            resolve_max_sessions(con, "http://localhost:8000/v1"),
            Some(4),
            "the other backends keep the default cap"
        );
    }

    /// `effective_max_sessions` is the seam the pool/client will call: it follows
    /// the active config, and reports unlimited when nothing is configured or no
    /// config is active at all.
    #[test]
    fn effective_max_sessions_follows_the_active_config() {
        {
            let installed = merged(
                "[orchestration.concurrency]\nmax_sessions = 3\n\"http://192.168.50.162:8080/v1\" = 1\n",
            );
            let _guard = ActiveConfigTestGuard::install(installed);
            assert_eq!(effective_max_sessions("http://localhost:8000/v1"), Some(3));
            assert_eq!(
                effective_max_sessions("http://192.168.50.162:8080/v1/"),
                Some(1),
                "the accessor normalizes the runtime URL too"
            );
        }
        {
            let _guard = ActiveConfigTestGuard::install(Config::default());
            assert_eq!(
                effective_max_sessions("http://localhost:8000/v1"),
                None,
                "an active config with no concurrency block must stay unlimited"
            );
        }

        // "No active config" is process-global state, so it is taken and restored
        // under the same lock the config guards use.
        let _serial = sandbox_env_test_lock();
        let previous = get_active();
        clear_active();
        let observed = effective_max_sessions("http://localhost:8000/v1");
        if let Some(cfg) = previous {
            set_active(cfg);
        }
        assert_eq!(observed, None, "no active config must never impose a cap");
    }

    /// A typo in this table is not ignored — `flatten` turns it into an override
    /// key. It can never match a backend, so it stays inert, but it is reported.
    #[test]
    fn concurrency_key_typo_becomes_an_inert_suspect_key() {
        let cfg = merged("[orchestration.concurrency]\nmax_session = 4\n");
        let con = &cfg.orchestration.concurrency;
        assert_eq!(
            con.max_sessions, None,
            "a typo'd `max_sessions` is NOT the default cap"
        );
        assert_eq!(
            con.by_backend.get("max_session"),
            Some(&4),
            "flatten lands the typo in the override table"
        );
        assert_eq!(
            con.suspect_backend_keys(),
            vec!["max_session".to_string()],
            "the typo must be flagged as not-a-URL"
        );
        assert_eq!(
            resolve_max_sessions(con, "http://localhost:8000/v1"),
            None,
            "an inert key must not impose a cap on any backend"
        );
    }

    #[test]
    fn suspect_backend_keys_flags_only_non_url_keys() {
        let con = ConcurrencyConfig {
            by_backend: [
                ("http://a/v1".to_string(), 1),
                ("https://b/v1/".to_string(), 2),
                (" ws://c/v1 ".to_string(), 3),
                ("max_sessions_typo".to_string(), 7),
            ]
            .into_iter()
            .collect(),
            max_sessions: Some(4),
        };
        assert_eq!(
            con.suspect_backend_keys(),
            vec![" ws://c/v1 ".to_string(), "max_sessions_typo".to_string()],
            "sorted, and http(s) keys are never suspects"
        );
        assert!(
            !con.is_unlimited(),
            "a table with overrides is never the unlimited default"
        );
    }

    /// Parity with `reasoning_effort_type_mismatch_is_a_toml_error_not_a_silent_default`:
    /// the shapes that cannot mean anything must fail the parse, and the verified
    /// TOML grammar of this table is pinned here.
    #[test]
    fn concurrency_malformed_keys_are_toml_errors_not_silent_defaults() {
        for (label, toml_str) in [
            (
                "unquoted URL key",
                "[orchestration.concurrency]\nhttp://host:8080/v1 = 2\n",
            ),
            (
                "negative cap",
                "[orchestration.concurrency]\nmax_sessions = -1\n",
            ),
            (
                "string where an integer belongs",
                "[orchestration.concurrency]\n\"http://a/v1\" = \"2\"\n",
            ),
            (
                "nested sub-table instead of a URL key",
                "[orchestration.concurrency.backends]\n\"http://a/v1\" = 2\n",
            ),
        ] {
            assert!(
                toml::from_str::<PartialConfig>(toml_str).is_err(),
                "{label}: must be a TOML parse error, never a silently dropped key"
            );
        }

        // The well-formed shape next to the failures above.
        assert!(
            toml::from_str::<PartialConfig>(
                "[orchestration.concurrency]\nmax_sessions = 4\n\"http://a/v1\" = 2\n"
            )
            .is_ok(),
            "the feature.md shape must parse"
        );
    }

    /// The "four places" rule for this key, scraped from this file: a block that
    /// is missing from the partial mirror or from `merge` is dropped **silently**
    /// by `toml`. The `marmel.toml.example` leg is owned by the docs task.
    #[test]
    fn concurrency_exists_in_config_partial_and_merge() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/config.rs"))
            .expect("src/config.rs must be readable");

        let slice = |start: &str, end: &str| -> String {
            let from = src
                .find(start)
                .unwrap_or_else(|| panic!("{start} must exist in src/config.rs"));
            let rest = &src[from..];
            let to = rest.find(end).unwrap_or(rest.len());
            rest[..to].to_string()
        };

        let orchestration_struct = slice(
            "pub struct OrchestrationConfig {",
            "impl OrchestrationConfig {",
        );
        let concurrency_struct =
            slice("pub struct ConcurrencyConfig {", "impl ConcurrencyConfig {");
        let partial_orchestration = slice(
            "struct PartialOrchestrationConfig {",
            "struct PartialConcurrencyConfig {",
        );
        let partial_concurrency = slice(
            "struct PartialConcurrencyConfig {",
            "fn merge(mut base: Config",
        );
        let merge_body = slice("fn merge(mut base: Config", "impl Config {");
        let load_body = slice(
            "pub fn load(explicit_path: Option<&str>)",
            "/// Tri-state reading",
        );
        // Receiver chains get split across lines by rustfmt, so the wiring
        // assertions compare whitespace-insensitively.
        let tight = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        let merge_wiring = tight(&merge_body);

        assert!(
            orchestration_struct.contains("pub concurrency: ConcurrencyConfig"),
            "OrchestrationConfig must declare the concurrency block"
        );
        assert!(
            concurrency_struct.contains("pub max_sessions: Option<usize>")
                && concurrency_struct.contains("#[serde(flatten)]")
                && concurrency_struct.contains("pub by_backend: HashMap<String, usize>"),
            "ConcurrencyConfig must carry Option<usize> plus the flattened URL table \
             that the same-table shape requires"
        );
        assert!(
            partial_orchestration.contains("pub concurrency: Option<PartialConcurrencyConfig>"),
            "PartialOrchestrationConfig must mirror the block as Option"
        );
        assert!(
            partial_concurrency.contains("pub max_sessions: Option<usize>")
                && partial_concurrency.contains("#[serde(flatten)]"),
            "PartialConcurrencyConfig must mirror the block, otherwise toml drops it"
        );
        assert!(
            merge_wiring.contains(&tight("if let Some(p_con) = p_orch.concurrency"))
                && merge_wiring.contains(&tight(
                    "base.orchestration.concurrency.max_sessions = Some(m)"
                ))
                && merge_wiring.contains(&tight("concurrency.by_backend.extend(p_con.by_backend)")),
            "merge must apply both max_sessions and the override table"
        );
        assert!(
            tight(&load_body).contains(&tight(
                "warn_suspect_backend_keys(&cfg.orchestration.concurrency)"
            )),
            "load must report the flatten typo hazard once per file read"
        );
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

    // ------------------------------------------------------------ reasoning effort
    //
    // Three `Option<String>` knobs: `Config::reasoning_effort` (top level),
    // `SpecialistConfig::reasoning_effort` (per role) and
    // `SpecialistConfig::validator_reasoning_effort` (per validator, next to
    // `validator_model`). All are reached through `PartialConfig`, which has no
    // `deny_unknown_fields` — so a key missing from the mirror or from `merge` is
    // dropped **silently** by `toml::from_str` (the `max_fix_loop_rounds` hazard
    // recorded in docs/research_config_parsing.md). These tests, plus the
    // source-scraping guard below, are what catches that drift.
    // The values are free-form pass-through: this layer performs no vocabulary
    // check and no normalization.

    #[test]
    fn reasoning_effort_absent_everywhere_stays_none() {
        // Unset must stay "not set" (⇒ the field is omitted from the request),
        // never a synthetic default.
        assert!(
            Config::default().reasoning_effort.is_none(),
            "the top-level default must be None"
        );
        let cfg = merged("backend_url = \"http://localhost:9000/v1\"\n");
        assert!(
            cfg.reasoning_effort.is_none(),
            "an absent top-level key keeps None"
        );

        let cfg = merged(
            r#"
            [orchestration.specialists.coder]
            module = "src/agents/coder.rs"
            "#,
        );
        let coder = cfg
            .orchestration
            .specialists
            .get("coder")
            .expect("coder present");
        assert!(
            coder.reasoning_effort.is_none(),
            "an absent per-role key keeps None"
        );
        assert!(
            coder.validator_reasoning_effort.is_none(),
            "an absent per-validator key keeps None"
        );
        assert!(SpecialistConfig::default().reasoning_effort.is_none());
        assert!(
            SpecialistConfig::default()
                .validator_reasoning_effort
                .is_none()
        );
    }

    #[test]
    fn reasoning_effort_top_level_parses() {
        let cfg = merged("reasoning_effort = \"medium\"\n");
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("medium"));

        // It sits next to the token ceiling without disturbing it: effort is a
        // dial, `max_thinking_tokens` stays the cap.
        let cfg = merged(
            r#"
            max_thinking_tokens = 16384
            reasoning_effort = "medium"
            "#,
        );
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("medium"));
        assert_eq!(cfg.max_thinking_tokens, 16384);
    }

    #[test]
    fn specialist_reasoning_effort_is_per_role_without_cross_contamination() {
        let cfg = merged(
            r#"
            [orchestration.specialists.coder]
            module = "src/agents/coder.rs"
            reasoning_effort = "high"

            [orchestration.specialists.researcher]
            module = "src/agents/researcher.rs"
            "#,
        );

        let coder = cfg
            .orchestration
            .specialists
            .get("coder")
            .expect("coder present");
        assert_eq!(coder.reasoning_effort.as_deref(), Some("high"));

        let researcher = cfg
            .orchestration
            .specialists
            .get("researcher")
            .expect("researcher present");
        assert!(
            researcher.reasoning_effort.is_none(),
            "a sibling role must not inherit another role's effort"
        );
        assert!(researcher.validator_reasoning_effort.is_none());

        assert!(
            cfg.reasoning_effort.is_none(),
            "a per-role value must not set the top-level key"
        );

        // The other direction: a top-level value is not copied down during
        // parsing — the specialist > top-level precedence is applied where the
        // effort is resolved, not in `merge`.
        let cfg = merged(
            r#"
            reasoning_effort = "medium"

            [orchestration.specialists.coder]
            module = "src/agents/coder.rs"
            "#,
        );
        assert_eq!(cfg.reasoning_effort.as_deref(), Some("medium"));
        assert!(
            cfg.orchestration
                .specialists
                .get("coder")
                .expect("coder present")
                .reasoning_effort
                .is_none(),
            "parsing must not flatten the top-level value into the role entry"
        );
    }

    #[test]
    fn validator_reasoning_effort_parses_without_touching_the_specialist() {
        let cfg = merged(
            r#"
            [orchestration.specialists.coder]
            module = "src/agents/coder.rs"
            model = "coder-9"
            validator_model = "validator-9"
            validator_reasoning_effort = "low"
            "#,
        );

        let coder = cfg
            .orchestration
            .specialists
            .get("coder")
            .expect("coder present");
        assert_eq!(coder.validator_reasoning_effort.as_deref(), Some("low"));
        assert_eq!(coder.validator_model.as_deref(), Some("validator-9"));
        assert_eq!(
            coder.reasoning_effort.as_deref(),
            None,
            "the validator knob must not set the specialist's own effort"
        );
        assert_eq!(coder.model.as_deref(), Some("coder-9"));
    }

    #[test]
    fn reasoning_effort_supplied_only_in_the_partial_survives_the_merge() {
        // The silent-drop guard: `PartialConfig` mirrors the key and `merge`
        // applies it, so a value that exists only in the file reaches the
        // resolved config instead of vanishing.
        assert_eq!(
            merged("reasoning_effort = \"high\"\n")
                .reasoning_effort
                .as_deref(),
            Some("high")
        );

        // A partial that omits the key must leave an already-resolved base value
        // alone (no silent reset), even when it sets other keys.
        let base = Config {
            reasoning_effort: Some("medium".to_string()),
            ..Default::default()
        };
        let partial: PartialConfig = toml::from_str("model = \"other-model\"\n").expect("parses");
        assert_eq!(
            merge(base, partial).reasoning_effort.as_deref(),
            Some("medium"),
            "an omitted key keeps the base value"
        );
        let partial: PartialConfig = toml::from_str("").expect("parses");
        let base = Config {
            reasoning_effort: Some("minimal".to_string()),
            ..Default::default()
        };
        assert_eq!(
            merge(base, partial).reasoning_effort.as_deref(),
            Some("minimal"),
            "an empty partial must never clear the key"
        );

        // NOTE (not asserted): role entries merge at *entry* granularity —
        // `merge` `extend`s the specialist table, so re-declaring
        // `[orchestration.specialists.coder]` replaces that whole entry. Unlike
        // the top-level key, a per-role knob must be repeated in the entry that
        // declares it.
    }

    #[test]
    fn reasoning_effort_values_are_preserved_verbatim() {
        // Documented vocabulary plus backend-specific spellings and odd casing:
        // this layer is a pass-through, so nothing is lowered, trimmed, mapped or
        // rejected.
        for value in [
            "none",
            "minimal",
            "low",
            "medium",
            "high",
            "xhigh",
            "Extra-High",
            " max ",
        ] {
            let cfg = merged(&format!("reasoning_effort = \"{value}\"\n"));
            assert_eq!(
                cfg.reasoning_effort.as_deref(),
                Some(value),
                "top level must store {value:?} verbatim (no vocabulary check, no normalization)"
            );

            let cfg = merged(&format!(
                "[orchestration.specialists.coder]\nreasoning_effort = \"{value}\"\n"
            ));
            assert_eq!(
                cfg.orchestration
                    .specialists
                    .get("coder")
                    .expect("coder present")
                    .reasoning_effort
                    .as_deref(),
                Some(value),
                "the per-role knob must store {value:?} verbatim"
            );

            let cfg = merged(&format!(
                "[orchestration.specialists.coder]\nvalidator_reasoning_effort = \"{value}\"\n"
            ));
            assert_eq!(
                cfg.orchestration
                    .specialists
                    .get("coder")
                    .expect("coder present")
                    .validator_reasoning_effort
                    .as_deref(),
                Some(value),
                "the per-validator knob must store {value:?} verbatim"
            );
        }

        // `"none"` is a *value*, not an unset: it stays Some("none") and must not
        // be collapsed into None by a vocabulary check.
        assert_eq!(
            merged("reasoning_effort = \"none\"\n")
                .reasoning_effort
                .as_deref(),
            Some("none")
        );
    }

    #[test]
    fn reasoning_effort_type_mismatch_is_a_toml_error_not_a_silent_default() {
        // Same contract as `load_toml_type_mismatch_is_an_error_not_a_silent_default`
        // (tests/test_session_config.rs:845-864), which mirrors `load`'s own parse
        // (`toml::from_str::<PartialConfig>` at src/config.rs:381): a *known* key
        // carrying the wrong TOML type fails the parse, and the exact diagnostic
        // text belongs to the `toml` crate and is deliberately not pinned.
        let err = toml::from_str::<PartialConfig>("reasoning_effort = 3\n")
            .expect_err("an integer where a string belongs must be rejected, not defaulted");
        assert!(!err.to_string().is_empty(), "the error must be reportable");

        for (label, toml_str) in [
            ("top level integer", "reasoning_effort = 3\n"),
            ("top level boolean", "reasoning_effort = true\n"),
            (
                "top level table",
                "reasoning_effort = { level = \"high\" }\n",
            ),
            (
                "specialist integer",
                "[orchestration.specialists.coder]\nreasoning_effort = 3\n",
            ),
            (
                "validator integer",
                "[orchestration.specialists.coder]\nvalidator_reasoning_effort = 3\n",
            ),
            (
                "validator array",
                "[orchestration.specialists.coder]\nvalidator_reasoning_effort = [\"low\"]\n",
            ),
        ] {
            assert!(
                toml::from_str::<PartialConfig>(toml_str).is_err(),
                "{label}: a mistyped `reasoning_effort` must be a TOML parse error"
            );
        }

        // Parity with another typed key: the rejection is not specific to
        // `reasoning_effort`.
        assert!(
            toml::from_str::<PartialConfig>("max_thinking_tokens = \"many\"\n").is_err(),
            "a mistyped `max_thinking_tokens` must be rejected the same way"
        );

        // And a bare string still parses in every position (the happy path next
        // to the failures above).
        assert!(
            toml::from_str::<PartialConfig>(
                "reasoning_effort = \"low\"\n[orchestration.specialists.coder]\nreasoning_effort = \"high\"\nvalidator_reasoning_effort = \"minimal\"\n"
            )
            .is_ok(),
            "well-typed effort keys must parse"
        );
    }

    /// The "four places" rule for `reasoning_effort`, scraped from this file: a key
    /// absent from `PartialConfig` or from `merge` is silently dropped by `toml`.
    /// Sibling of
    /// `every_session_key_exists_in_config_partial_merge_and_the_example`, minus the
    /// `marmel.toml.example` leg — that file is owned by the docs task.
    #[test]
    fn reasoning_effort_exists_in_config_default_partial_and_merge() {
        let src = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/src/config.rs"))
            .expect("src/config.rs must be readable");

        let slice = |start: &str, end: &str| -> String {
            let from = src
                .find(start)
                .unwrap_or_else(|| panic!("{start} must exist in src/config.rs"));
            let rest = &src[from..];
            let to = rest.find(end).unwrap_or(rest.len());
            rest[..to].to_string()
        };

        let specialist_struct = slice("pub struct SpecialistConfig {", "pub struct Config {");
        let config_struct = slice("pub struct Config {", "impl Default for Config {");
        let defaults = slice("impl Default for Config {", "static ACTIVE_CONFIG");
        let partial_struct = slice("struct PartialConfig {", "struct PartialMonitoringConfig {");
        let merge_body = slice("fn merge(mut base: Config", "impl Config {");

        assert!(
            config_struct.contains("pub reasoning_effort: Option<String>"),
            "Config must declare reasoning_effort"
        );
        assert!(
            defaults.contains("reasoning_effort: None"),
            "impl Default for Config must set reasoning_effort: None"
        );
        assert!(
            partial_struct.contains("pub reasoning_effort: Option<String>"),
            "PartialConfig must mirror reasoning_effort as Option, otherwise the key is \
             silently dropped by toml"
        );
        assert!(
            merge_body.contains("partial.reasoning_effort")
                && merge_body.contains("base.reasoning_effort ="),
            "merge must apply reasoning_effort"
        );
        assert!(
            specialist_struct.contains("pub reasoning_effort: Option<String>"),
            "SpecialistConfig must declare reasoning_effort"
        );
        assert!(
            specialist_struct.contains("pub validator_reasoning_effort: Option<String>"),
            "SpecialistConfig must declare validator_reasoning_effort next to validator_model"
        );
    }

    // Typed config knobs that the harness actually reads: `command_timeout_secs`
    // and `sandbox_disabled`. These tests are pty-free: the sandbox outcome is
    // asserted through `should_apply_sandbox(&SandboxInputs)`, never by executing
    // a command.

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

    /// The knob reaches `SandboxInputs::opt_out` and therefore the decision.
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

    /// Knob off plus an env opt-out ⇒ the env opt-out still applies.
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

    /// Knob on plus an env value that is not an opt-out ⇒ Skip; with both set the
    /// outcome is the same Skip (never an AND of the two sources); with nothing
    /// opted out the sandbox stays on.
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

    /// The configured `command_timeout_secs` is what the harness applies.
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
    // drift; `max_fix_loop_rounds` is the in-repo proof that it happens.

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

    /// Every key must exist in all four places, or it is unsettable.
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

    // ------------------------------------------------- sandbox extra paths key
    //
    // `sandbox_extra_readonly_paths` is the first **top-level array-of-strings**
    // key in `PartialConfig`, so it follows the `#[serde(default)]` + plain `Vec`
    // mirror idiom of `PartialOrchestrationConfig::mcp_servers` and the same
    // replace-if-non-empty merge. Same drift risk as above: a key missing from the
    // mirror or from `merge` is dropped **silently** by `toml`. The plumbing into
    // the Landlock ruleset is a separate change; these tests cover the config layer
    // only.

    #[test]
    fn sandbox_extra_readonly_paths_defaults_to_empty() {
        assert!(
            Config::default().sandbox_extra_readonly_paths.is_empty(),
            "the built-in sandbox allow-list must stay authoritative unless the user \
             opts into extra paths"
        );
        assert!(
            partial_from("backend_url = \"http://localhost:9000/v1\"\n")
                .sandbox_extra_readonly_paths
                .is_empty(),
            "an absent `sandbox_extra_readonly_paths` keeps the default"
        );
        assert!(
            partial_from("").sandbox_extra_readonly_paths.is_empty(),
            "an empty file keeps the default"
        );
    }

    #[test]
    fn sandbox_extra_readonly_paths_parses_and_merges() {
        let cfg = merged("sandbox_extra_readonly_paths = [\"/nix/store\"]\n");
        assert_eq!(
            cfg.sandbox_extra_readonly_paths,
            vec!["/nix/store".to_string()]
        );

        let cfg = merged("sandbox_extra_readonly_paths = [\"/nix/store\", \"/gnu/store\"]\n");
        assert_eq!(
            cfg.sandbox_extra_readonly_paths,
            vec!["/nix/store".to_string(), "/gnu/store".to_string()]
        );

        // Merge semantics are `orchestration.mcp_servers`' (replace, not append):
        // the default is empty, so a non-empty file value becomes the value verbatim.
        assert_eq!(
            merged(
                "sandbox_extra_readonly_paths = [\"/nix/store\"]\n\
                 [orchestration]\n\
                 mcp_servers = [\"fs\"]\n"
            )
            .sandbox_extra_readonly_paths,
            vec!["/nix/store".to_string()],
            "the key must survive alongside a neighbouring table"
        );
    }

    #[test]
    fn sandbox_extra_readonly_paths_empty_array_is_a_noop_and_cannot_clear() {
        // `#[serde(default)]` on a plain `Vec` cannot tell an absent key from an
        // explicit `[]`, so the `!is_empty()` guard makes `[]` a no-op — the same
        // documented limitation as `orchestration.mcp_servers`.
        assert!(
            merged("sandbox_extra_readonly_paths = []\n")
                .sandbox_extra_readonly_paths
                .is_empty()
        );
        let cfg = merge(
            Config {
                sandbox_extra_readonly_paths: vec!["/gnu/store".to_string()],
                ..Config::default()
            },
            toml::from_str::<PartialConfig>("sandbox_extra_readonly_paths = []\n").expect("parses"),
        );
        assert_eq!(
            cfg.sandbox_extra_readonly_paths,
            vec!["/gnu/store".to_string()],
            "an explicit empty array must not clear an existing list"
        );
    }
    #[test]
    fn sandbox_extra_readonly_paths_type_mismatch_is_a_toml_error() {
        // Known key, wrong TOML type: hard parse error, never a silent default
        // (same contract as `reasoning_effort_type_mismatch_is_a_toml_error_not_a_silent_default`).
        for (label, toml_str) in [
            (
                "bare string instead of array",
                "sandbox_extra_readonly_paths = \"/nix/store\"\n",
            ),
            ("boolean", "sandbox_extra_readonly_paths = true\n"),
            ("integer array", "sandbox_extra_readonly_paths = [1, 2]\n"),
            (
                "mixed array",
                "sandbox_extra_readonly_paths = [\"/nix/store\", 2]\n",
            ),
            (
                "array of tables",
                "sandbox_extra_readonly_paths = [{ path = \"/nix/store\" }]\n",
            ),
        ] {
            assert!(
                toml::from_str::<PartialConfig>(toml_str).is_err(),
                "{label}: a mistyped `sandbox_extra_readonly_paths` must be a TOML \
                 parse error, not a silently dropped key"
            );
        }
    }

    /// The "four places" rule for `sandbox_extra_readonly_paths`, scraped from this
    //  file, plus the `marmel.toml.example` leg this key must satisfy (the file is
    //  edited in the same change). Sibling of
    //  `every_session_key_exists_in_config_partial_merge_and_the_example`.
    #[test]
    fn sandbox_extra_readonly_paths_exists_in_config_default_partial_merge_and_example() {
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

        assert!(
            config_struct.contains("pub sandbox_extra_readonly_paths: Vec<String>"),
            "Config must declare sandbox_extra_readonly_paths next to sandbox_disabled"
        );
        assert!(
            config_struct
                .find("pub sandbox_extra_readonly_paths: Vec<String>")
                .unwrap()
                > config_struct.find("pub sandbox_disabled: bool").unwrap(),
            "sandbox_extra_readonly_paths must sit next to sandbox_disabled so the \
             sandbox grouping is obvious"
        );
        assert!(
            defaults.contains("sandbox_extra_readonly_paths: Vec::new()"),
            "impl Default for Config must set sandbox_extra_readonly_paths: Vec::new()"
        );
        // Mirror must carry `#[serde(default)]` (a non-Option mirror without it is a
        // required key) and must not be renamed: the TOML key is the snake_case
        // field name verbatim, as everywhere else in this file.
        let mirror_at = partial_struct
            .find("pub sandbox_extra_readonly_paths: Vec<String>")
            .expect("PartialConfig must mirror sandbox_extra_readonly_paths as Vec<String>");
        let before_mirror = partial_struct[..mirror_at].trim_end();
        assert!(
            before_mirror.ends_with("#[serde(default)]"),
            "PartialConfig must mirror sandbox_extra_readonly_paths with \
             #[serde(default)] immediately above the field"
        );
        assert!(
            !partial_struct.contains("rename"),
            "no serde rename: config keys are snake_case field names used verbatim"
        );
        assert!(
            merge_body.contains("partial.sandbox_extra_readonly_paths")
                && merge_body.contains("base.sandbox_extra_readonly_paths ="),
            "merge must apply sandbox_extra_readonly_paths, otherwise the key is \
             silently dropped by toml"
        );
        // Receiver chains get split across lines by rustfmt, so the list-merge
        // shape is compared whitespace-insensitively.
        let tight = |s: &str| -> String { s.chars().filter(|c| !c.is_whitespace()).collect() };
        assert!(
            tight(&merge_body).contains(
                "if!partial.sandbox_extra_readonly_paths.is_empty(){\
                 base.sandbox_extra_readonly_paths=partial.sandbox_extra_readonly_paths;}"
            ),
            "merge must keep the replace-if-non-empty list semantics documented for \
             this key (same as orchestration.mcp_servers)"
        );
        assert!(
            example.contains("# sandbox_extra_readonly_paths ="),
            "marmel.toml.example must document sandbox_extra_readonly_paths"
        );
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
