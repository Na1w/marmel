//! Configuration schema, TOML parsing, and path expansion for marmel.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;

/// Default delegation depth bound.
pub const DEFAULT_MAX_RECURSION_DEPTH: usize = 3;

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
}

impl Default for MonitoringConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            repetition_threshold: 5,
            min_pattern_len: 5,
        }
    }
}

/// A single per-specialist configuration entry from `[orchestration.specialists]`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct SpecialistConfig {
    /// Source module path.
    pub module: String,
    /// Allowed tool namespaces.
    pub tools: Vec<String>,
    /// Optional model override for this specialist.
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
    /// Whether automated validation is enabled for this specialist (default: true).
    #[serde(alias = "auto_validate", alias = "enable_validation")]
    pub enable_validator: Option<bool>,
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
    pub command_timeout_secs: u64,
    pub max_repetition_threshold: usize,
    pub enable_xml_rescue: bool,
    pub ui_mode: String,
    /// Detailed debug logging to debug.log.
    pub debug: bool,
    /// Resilience harness thresholds.
    pub monitoring: Option<MonitoringConfig>,
    /// Orchestration block.
    pub orchestration: OrchestrationConfig,
    /// Configured external MCP servers (`[mcp_servers.<name>]`).
    pub mcp_servers: HashMap<String, crate::mcp::McpServerConfig>,
}

impl Default for Config {
    fn default() -> Self {
        Config {
            backend_url: "http://localhost:8000/v1".to_string(),
            auth_token: String::new(),
            model: "llama3.1-8b-instruct".to_string(),
            temperature: 0.7,
            top_p: 0.9,
            frequency_penalty: 0.0,
            presence_penalty: 0.0,
            max_context_tokens: 8192,
            system_prompt_path: PathBuf::from("prompts/system.md"),
            preserve_thinking: true,
            command_timeout_secs: 60,
            max_repetition_threshold: 3,
            enable_xml_rescue: true,
            ui_mode: "tui".to_string(),
            debug: false,
            monitoring: Some(MonitoringConfig::default()),
            orchestration: OrchestrationConfig::default_depth(),
            mcp_servers: HashMap::new(),
        }
    }
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

    cfg.expand_paths();
    Ok(cfg)
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
    pub max_repetition_threshold: Option<usize>,
    pub enable_xml_rescue: Option<bool>,
    pub ui_mode: Option<String>,
    pub monitoring: Option<PartialMonitoringConfig>,
    pub orchestration: Option<PartialOrchestrationConfig>,
    #[serde(default)]
    pub mcp_servers: HashMap<String, crate::mcp::McpServerConfig>,
}

#[derive(Debug, Clone, Deserialize, Default)]
struct PartialMonitoringConfig {
    pub enabled: Option<bool>,
    pub repetition_threshold: Option<usize>,
    pub min_pattern_len: Option<usize>,
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
    if let Some(v) = partial.max_repetition_threshold {
        base.max_repetition_threshold = v;
    }
    if let Some(v) = partial.enable_xml_rescue {
        base.enable_xml_rescue = v;
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

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from).or_else(|| {
        #[cfg(unix)]
        {
            let pw = unsafe { libc::getpwuid(libc::getuid()) };
            if pw.is_null() {
                return None;
            }
            let dir = unsafe { std::ffi::CStr::from_ptr((*pw).pw_dir) };
            Some(PathBuf::from(dir.to_string_lossy().into_owned()))
        }
        #[cfg(not(unix))]
        {
            None
        }
    })
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
}
