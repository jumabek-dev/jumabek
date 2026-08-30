use serde::{Deserialize, Serialize};

use crate::{MethodInfo, ModuleMetadata, SkillError, SkillOutput};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteParams {
    pub method: String,
    pub args: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub caller: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillRequest {
    pub id: u64,
    pub method: String, // execute | get_metadata | health_check | available_methods
    pub params: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SkillResponsePayload {
    Metadata(ModuleMetadata),
    Methods(Vec<MethodInfo>),
    Health(bool),
    Output(SkillOutput),
    Error(SkillError),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillResponse {
    pub id: u64,
    pub payload: SkillResponsePayload,
}
