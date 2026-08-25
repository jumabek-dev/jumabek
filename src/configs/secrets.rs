use serde::{Deserialize, Serialize};

use crate::configs::find_file;
use crate::core::intelligence::Level;
use crate::error::{JumabekError, JumabekResult};

pub const ENV_API_KEY: &str = "JUMABEK_API_KEY";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Secrets {
    #[serde(default)]
    pub llm: LlmSecrets,
    #[serde(default)]
    pub voice: Option<VoiceSecrets>,
    #[serde(default)]
    pub inbox: Option<InboxSecrets>,
    #[serde(default)]
    pub skills: std::collections::BTreeMap<String, std::collections::BTreeMap<String, String>>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InboxSecrets {
    #[serde(default)]
    pub tokens: std::collections::BTreeMap<String, String>,
}

pub fn inbox_tokens() -> std::collections::BTreeMap<String, String> {
    load()
        .ok()
        .flatten()
        .and_then(|s| s.inbox)
        .map(|inbox| inbox.tokens)
        .unwrap_or_default()
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LlmSecrets {
    #[serde(default)]
    pub api_key: Option<String>,
    #[serde(default)]
    pub levels: std::collections::BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VoiceSecrets {
    pub groq_api_key: String,
}

pub fn load() -> JumabekResult<Option<Secrets>> {
    let Ok(path) = find_file("secrets.toml") else {
        return Ok(None);
    };

    warn_if_world_readable(&path);

    let text = std::fs::read_to_string(&path)
        .map_err(|e| JumabekError::ConfigError(format!("cannot read {}: {}", path.display(), e)))?;

    let secrets: Secrets = toml::from_str(&text).map_err(|e| {
        JumabekError::ConfigError(format!("invalid secrets at {}: {}", path.display(), e))
    })?;

    Ok(Some(secrets))
}

pub fn groq_api_key() -> JumabekResult<Option<String>> {
    if let Ok(key) = std::env::var("JUMABEK_GROQ_API_KEY") {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(Some(key));
        }
    }

    Ok(load()?
        .and_then(|s| s.voice)
        .map(|v| v.groq_api_key.trim().to_string())
        .filter(|k| !k.is_empty()))
}

pub fn resolve_api_key() -> JumabekResult<String> {
    if let Ok(key) = std::env::var(ENV_API_KEY) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }

    Ok(load()?
        .and_then(|secrets| secrets.llm.api_key)
        .map(|key| key.trim().to_string())
        .unwrap_or_default())
}

pub fn resolve_api_key_for(level: Level) -> JumabekResult<String> {
    let env_var = format!("JUMABEK_API_KEY_{}", level.id().to_ascii_uppercase());
    if let Ok(key) = std::env::var(&env_var) {
        let key = key.trim().to_string();
        if !key.is_empty() {
            return Ok(key);
        }
    }

    let per_level = load()?
        .and_then(|secrets| secrets.llm.levels.get(level.id()).cloned())
        .map(|key| key.trim().to_string())
        .filter(|key| !key.is_empty());

    if let Some(key) = per_level {
        return Ok(key);
    }

    resolve_api_key()
}

#[cfg(unix)]
fn warn_if_world_readable(path: &std::path::Path) {
    use std::os::unix::fs::PermissionsExt;

    if let Ok(meta) = std::fs::metadata(path) {
        let mode = meta.permissions().mode();
        if mode & 0o077 != 0 {
            eprintln!(
                "[configs] warning: {} is readable by other users (mode {:o}); run: chmod 600 {}",
                path.display(),
                mode & 0o777,
                path.display()
            );
        }
    }
}

#[cfg(not(unix))]
fn warn_if_world_readable(_path: &std::path::Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Secrets {
        toml::from_str(text).expect("secrets should parse")
    }

    #[test]
    fn a_secrets_file_without_a_key_is_a_statement_not_a_syntax_error() {
        assert_eq!(parse("[llm]\n").llm.api_key, None);
        assert_eq!(parse("[inbox.tokens]\n").llm.api_key, None);
        assert_eq!(parse("").llm.api_key, None);
    }

    #[test]
    fn a_key_that_is_there_is_still_read() {
        assert_eq!(
            parse("[llm]\napi_key = \"sk-abc\"\n")
                .llm
                .api_key
                .as_deref(),
            Some("sk-abc")
        );
    }

    #[test]
    fn inbox_tokens_survive_a_file_with_no_llm_section() {
        let secrets = parse("[inbox.tokens]\ntelegram = \"0123456789012345678901234\"\n");
        assert!(secrets.inbox.unwrap().tokens.contains_key("telegram"));
    }

    #[test]
    fn a_level_key_is_read_from_its_own_table() {
        let secrets = parse("[llm.levels]\nmedium = \"ollama-cloud-key\"\n");
        assert_eq!(
            secrets.llm.levels.get("medium").map(String::as_str),
            Some("ollama-cloud-key")
        );
        assert!(!secrets.llm.levels.contains_key("low"));
    }
}
