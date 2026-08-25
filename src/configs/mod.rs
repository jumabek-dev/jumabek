pub mod secrets;

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::core::intelligence::Level;
use crate::core::task::InterfaceMode;
use crate::error::{JumabekError, JumabekResult};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub memory: MemorySection,
    pub llm: LlmSection,
    pub agent: AgentSection,
    #[serde(default)]
    pub preflight: PreflightSection,
    #[serde(default)]
    pub inbox: InboxSection,
    #[serde(default)]
    pub skills: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,

    #[serde(skip)]
    pub system_prompt: String,
    #[serde(skip)]
    pub system_prompt_file: PathBuf,
    #[serde(skip)]
    pub api_key: String,
    #[serde(skip)]
    pub level_api_keys: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemorySection {
    #[serde(default = "default_db_path")]
    pub db_path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmSection {
    pub model: String,
    pub base_uri: String,
    #[serde(default = "default_prompt_path")]
    pub system_prompt_path: String,
    #[serde(default = "default_context_limit")]
    pub context_token_limit: u32,
    #[serde(default = "default_retry_max_retries")]
    pub retry_max_retries: u32,
    #[serde(default = "default_retry_initial_delay_ms")]
    pub retry_initial_delay_ms: u64,
    #[serde(default = "default_request_timeout")]
    pub request_timeout_sec: u64,
    #[serde(default)]
    pub reasoning_effort: String,
    #[serde(default)]
    pub protocol: String,
    #[serde(default = "default_max_tokens")]
    pub max_tokens: u32,
    #[serde(default)]
    pub intelligence: IntelligenceSection,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct IntelligenceSection {
    #[serde(default)]
    pub low: String,
    #[serde(default)]
    pub medium: String,
    #[serde(default)]
    pub high: String,
    #[serde(default)]
    pub default: String,
    #[serde(default)]
    pub endpoints: std::collections::BTreeMap<String, LevelEndpoint>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LevelEndpoint {
    #[serde(default)]
    pub base_uri: Option<String>,
    #[serde(default)]
    pub reasoning_effort: Option<String>,
    #[serde(default)]
    pub structured_output: Option<bool>,
    #[serde(default)]
    pub context_token_limit: Option<u32>,
    #[serde(default)]
    pub protocol: Option<String>,
    #[serde(default)]
    pub max_tokens: Option<u32>,
}

impl IntelligenceSection {
    pub fn enabled(&self) -> bool {
        Level::ALL
            .iter()
            .all(|level| !self.model(*level).is_empty())
    }

    pub fn model(&self, level: Level) -> &str {
        match level {
            Level::Low => self.low.trim(),
            Level::Medium => self.medium.trim(),
            Level::High => self.high.trim(),
        }
    }

    pub fn endpoint(&self, level: Level) -> Option<&LevelEndpoint> {
        self.endpoints.get(level.id())
    }

    pub fn starting_level(&self) -> Level {
        Level::parse(&self.default).unwrap_or_default()
    }

    pub fn problems(&self) -> Vec<String> {
        let named: Vec<Level> = Level::ALL
            .iter()
            .copied()
            .filter(|level| !self.model(*level).is_empty())
            .collect();

        if named.is_empty() {
            return Vec::new();
        }

        let mut problems = Vec::new();

        for level in Level::ALL {
            if self.model(level).is_empty() {
                problems.push(format!(
                    "[llm.intelligence] names some levels but not '{}', so switching is off",
                    level
                ));
            }
        }

        if !self.default.trim().is_empty() && Level::parse(&self.default).is_none() {
            problems.push(format!(
                "[llm.intelligence] default = '{}' is not one of low, medium, high",
                self.default.trim()
            ));
        }

        problems
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSection {
    #[serde(default = "default_max_iterations")]
    pub max_iterations: u32,
    #[serde(default = "default_max_fix_iterations")]
    pub max_fix_iterations: u32,
    #[serde(default = "default_interface")]
    pub interface: String,
    #[serde(default = "default_skill_timeout")]
    pub skill_timeout_sec: u64,
    #[serde(default)]
    pub voice_name: Option<String>,
    #[serde(default = "default_language")]
    pub language: String,
    #[serde(default = "default_carry_over")]
    pub carry_over_messages: u32,
}

fn default_carry_over() -> u32 {
    30
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboxSection {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default = "default_inbox_port")]
    pub port: u16,
    #[serde(default = "default_inbox_timeout")]
    pub ask_timeout_sec: u64,
    #[serde(default)]
    pub grants: std::collections::BTreeMap<String, crate::core::task::Grant>,
}

impl Default for InboxSection {
    fn default() -> Self {
        InboxSection {
            enabled: false,
            port: default_inbox_port(),
            ask_timeout_sec: default_inbox_timeout(),
            grants: std::collections::BTreeMap::new(),
        }
    }
}

fn default_inbox_port() -> u16 {
    20129
}

fn default_inbox_timeout() -> u64 {
    300
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PreflightSection {
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default = "default_image")]
    pub image: String,
    #[serde(default)]
    pub images: std::collections::BTreeMap<String, String>,
    #[serde(default = "default_build_cpu")]
    pub build_cpu: String,
    #[serde(default = "default_build_memory")]
    pub build_memory: String,
    #[serde(default = "default_run_cpu")]
    pub run_cpu: String,
    #[serde(default = "default_run_memory")]
    pub run_memory: String,
    #[serde(default = "default_build_timeout")]
    pub build_timeout_sec: u64,
    #[serde(default)]
    pub allow_without_docker: bool,
}

impl Default for PreflightSection {
    fn default() -> Self {
        PreflightSection {
            enabled: true,
            image: default_image(),
            images: std::collections::BTreeMap::new(),
            build_cpu: default_build_cpu(),
            build_memory: default_build_memory(),
            run_cpu: default_run_cpu(),
            run_memory: default_run_memory(),
            build_timeout_sec: default_build_timeout(),
            allow_without_docker: false,
        }
    }
}

impl PreflightSection {
    pub fn image_for(&self, language: crate::core::languages::Language) -> &str {
        if let Some(named) = self.images.get(language.id()) {
            return named;
        }
        if language.needs_sdk() {
            return &self.image;
        }
        language.default_image()
    }
}

fn default_true() -> bool {
    true
}
fn default_image() -> String {
    "rust:1-slim".to_string()
}
fn default_build_cpu() -> String {
    "2".to_string()
}
fn default_build_memory() -> String {
    "2g".to_string()
}
fn default_run_cpu() -> String {
    "0.5".to_string()
}
fn default_run_memory() -> String {
    "256m".to_string()
}
fn default_build_timeout() -> u64 {
    600
}

fn default_db_path() -> String {
    "~/.jumabek/jumabek.db".to_string()
}
fn default_prompt_path() -> String {
    "./prompt.md".to_string()
}
fn default_context_limit() -> u32 {
    128_000
}
fn default_retry_max_retries() -> u32 {
    3
}
fn default_request_timeout() -> u64 {
    180
}
fn default_max_tokens() -> u32 {
    8192
}
fn default_retry_initial_delay_ms() -> u64 {
    1000
}
fn default_max_iterations() -> u32 {
    10
}
fn default_max_fix_iterations() -> u32 {
    5
}
fn default_interface() -> String {
    "cli".to_string()
}
fn default_skill_timeout() -> u64 {
    360
}
fn default_language() -> String {
    "ru".to_string()
}

pub fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn jumabek_dir() -> Option<PathBuf> {
    home_dir().map(|home| home.join(".jumabek"))
}

pub fn expand_tilde(raw: &str) -> PathBuf {
    if let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\"))
        && let Some(home) = home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(raw)
}

pub fn find_file(filename: &str) -> JumabekResult<PathBuf> {
    let mut checked = Vec::new();

    for dir in [Some(PathBuf::from(".")), jumabek_dir()]
        .into_iter()
        .flatten()
    {
        let path = dir.join(filename);
        if path.exists() {
            return Ok(path);
        }
        checked.push(path.display().to_string());
    }

    Err(JumabekError::ConfigError(format!(
        "{} not found. Checked: {}",
        filename,
        checked.join(", ")
    )))
}

impl Config {
    pub fn load() -> JumabekResult<(Self, PathBuf)> {
        let path = find_file("config.toml")?;

        let text = std::fs::read_to_string(&path).map_err(|e| {
            JumabekError::ConfigError(format!("cannot read {}: {}", path.display(), e))
        })?;

        let mut config: Config = toml::from_str(&text).map_err(|e| {
            JumabekError::ConfigError(format!("invalid config at {}: {}", path.display(), e))
        })?;

        let base = path.parent().unwrap_or(Path::new("."));
        config.system_prompt_file = resolve_prompt_path(base, &config.llm.system_prompt_path);
        config.system_prompt = load_system_prompt(&config.system_prompt_file)?;
        config.api_key = secrets::resolve_api_key()?;

        for level in Level::ALL {
            let key = secrets::resolve_api_key_for(level)?;
            if !key.is_empty() {
                config.level_api_keys.insert(level.id().to_string(), key);
            }
        }

        Ok((config, path))
    }

    pub fn api_key_for(&self, level: Level) -> &str {
        self.level_api_keys
            .get(level.id())
            .map(String::as_str)
            .unwrap_or(&self.api_key)
    }

    pub fn settings_for_skill(&self, name: &str) -> std::collections::BTreeMap<String, String> {
        let mut merged = self.skills.get(name).cloned().unwrap_or_default();

        if let Ok(Some(secrets)) = secrets::load()
            && let Some(section) = secrets.skills.get(name)
        {
            for (key, value) in section {
                merged.insert(key.clone(), value.clone());
            }
        }

        merged
    }

    pub fn db_path(&self) -> PathBuf {
        expand_tilde(&self.memory.db_path)
    }

    pub fn interface_mode(&self) -> JumabekResult<InterfaceMode> {
        match self.agent.interface.to_lowercase().as_str() {
            "cli" => Ok(InterfaceMode::Cli),
            "voice" => Ok(InterfaceMode::Voice),
            other => Err(JumabekError::ConfigError(format!(
                "unknown interface '{}', expected 'cli' or 'voice'",
                other
            ))),
        }
    }
}

fn resolve_prompt_path(base: &Path, raw: &str) -> PathBuf {
    let candidate = expand_tilde(raw);
    if candidate.is_absolute() {
        candidate
    } else {
        base.join(candidate)
    }
}

fn load_system_prompt(path: &Path) -> JumabekResult<String> {
    let text = std::fs::read_to_string(path).map_err(|e| {
        JumabekError::ConfigError(format!(
            "cannot read system prompt at {}: {}",
            path.display(),
            e
        ))
    })?;

    if text.trim().is_empty() {
        return Err(JumabekError::ConfigError(format!(
            "system prompt at {} is empty",
            path.display()
        )));
    }

    Ok(text)
}

#[cfg(test)]
mod intelligence_tests {
    use super::*;

    fn section(low: &str, medium: &str, high: &str, default: &str) -> IntelligenceSection {
        IntelligenceSection {
            low: low.to_string(),
            medium: medium.to_string(),
            high: high.to_string(),
            default: default.to_string(),
            endpoints: std::collections::BTreeMap::new(),
        }
    }

    #[test]
    fn a_config_that_names_nothing_keeps_the_single_model() {
        let none = section("", "", "", "");
        assert!(!none.enabled());
        assert!(none.problems().is_empty(), "silence is not a mistake");
    }

    #[test]
    fn naming_all_three_turns_switching_on() {
        let all = section("haiku", "sonnet", "opus", "");
        assert!(all.enabled());
        assert!(all.problems().is_empty());
        assert_eq!(all.model(Level::High), "opus");
    }

    #[test]
    fn naming_only_some_is_reported_rather_than_half_applied() {
        let half = section("haiku", "", "opus", "");
        assert!(
            !half.enabled(),
            "switching ran with a level that has no model"
        );
        assert_eq!(half.problems().len(), 1);
        assert!(half.problems()[0].contains("medium"));
    }

    #[test]
    fn the_starting_level_is_the_middle_one_unless_named() {
        assert_eq!(section("a", "b", "c", "").starting_level(), Level::Medium);
        assert_eq!(section("a", "b", "c", "low").starting_level(), Level::Low);
    }

    #[test]
    fn a_default_nobody_understands_is_said_out_loud() {
        let odd = section("a", "b", "c", "genius");
        assert_eq!(odd.starting_level(), Level::Medium);
        assert_eq!(odd.problems().len(), 1);
        assert!(odd.problems()[0].contains("genius"));
    }

    #[test]
    fn a_level_without_an_endpoint_override_has_none() {
        let plain = section("a", "b", "c", "");
        assert!(plain.endpoint(Level::Low).is_none());
    }

    #[test]
    fn a_level_with_an_endpoint_override_is_found_by_its_own_name() {
        let mut with_endpoint = section("a", "b", "c", "");
        with_endpoint.endpoints.insert(
            "low".to_string(),
            LevelEndpoint {
                base_uri: Some("http://localhost:11434/v1".to_string()),
                reasoning_effort: Some("none".to_string()),
                structured_output: None,
                context_token_limit: None,
                protocol: None,
                max_tokens: None,
            },
        );

        let found = with_endpoint.endpoint(Level::Low).expect("low was set");
        assert_eq!(found.base_uri.as_deref(), Some("http://localhost:11434/v1"));
        assert!(with_endpoint.endpoint(Level::Medium).is_none());
    }

    #[test]
    fn a_level_can_override_protocol_and_max_tokens() {
        let mut with_endpoint = section("a", "b", "c", "");
        with_endpoint.endpoints.insert(
            "high".to_string(),
            LevelEndpoint {
                base_uri: None,
                reasoning_effort: None,
                structured_output: None,
                context_token_limit: None,
                protocol: Some("anthropic".to_string()),
                max_tokens: Some(16_000),
            },
        );

        let found = with_endpoint.endpoint(Level::High).expect("high was set");
        assert_eq!(found.protocol.as_deref(), Some("anthropic"));
        assert_eq!(found.max_tokens, Some(16_000));
    }
}
