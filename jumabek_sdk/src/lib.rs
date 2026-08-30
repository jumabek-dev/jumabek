pub mod protocol;
pub mod runtime;

tokio::task_local! {
    pub(crate) static CALLER: Option<String>;
}

pub fn caller() -> Option<String> {
    CALLER.try_with(|id| id.clone()).ok().flatten()
}

use core::fmt;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModuleMetadata {
    pub name: String,
    pub version: String,
    pub description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MethodInfo {
    pub method: String,
    pub description: String,
    pub args_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkillOutput {
    Text(String),
    Json(serde_json::Value),
    Binary(Vec<u8>),
    Empty,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkillError {
    NotFound(String),
    ExecutionFailed(String),
    InvalidArgs(String),
    Recoverable(String),
    Fatal(String),
}

impl fmt::Display for SkillError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SkillError::NotFound(msg) => write!(f, "Skill not found: {}", msg),
            SkillError::ExecutionFailed(msg) => write!(f, "Execution failed: {}", msg),
            SkillError::InvalidArgs(msg) => write!(f, "Invalid arguments provided: {}", msg),
            SkillError::Recoverable(msg) => write!(f, "Recoverable error: {}", msg),
            SkillError::Fatal(msg) => write!(f, "Fatal error: {}", msg),
        }
    }
}

impl std::error::Error for SkillError {}

impl From<std::io::Error> for SkillError {
    fn from(value: std::io::Error) -> Self {
        SkillError::ExecutionFailed(value.to_string())
    }
}

#[async_trait::async_trait]
pub trait SkillModule: Send + Sync {
    fn get_metadata(&self) -> &ModuleMetadata;
    fn health_check(&self) -> bool;
    async fn execute(&self, method: &str, args: &str) -> Result<SkillOutput, SkillError>;
    fn available_methods(&self) -> Vec<MethodInfo>;
}
