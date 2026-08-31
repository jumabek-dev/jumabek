use serde::{Deserialize, Deserializer, Serialize};

fn flexible_string_vec<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::Array(items) => items
            .into_iter()
            .map(|item| match item {
                serde_json::Value::String(text) => text,
                other => other.to_string(),
            })
            .collect(),
        serde_json::Value::String(text) => vec![text],
        serde_json::Value::Null => Vec::new(),
        other => vec![other.to_string()],
    })
}

fn flexible_string<'de, D>(deserializer: D) -> Result<String, D::Error>
where
    D: Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(match value {
        serde_json::Value::String(text) => text,
        serde_json::Value::Null => String::new(),
        other => other.to_string(),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemInfo {
    pub os: String,
    pub shell: String,
    pub current_time: String,
    pub cwd: String,
    pub jumabek_home: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskObjectSkillMethod {
    pub method: String,
    pub description: String,
    pub args_description: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskObjectSkill {
    pub name: String,
    pub description: String,
    pub available_methods: Vec<TaskObjectSkillMethod>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Constraints {
    pub max_iterations: u32,
    pub max_fix_iterations: u32,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Grant {
    #[serde(default, deserialize_with = "flexible_string_vec")]
    pub skills: Vec<String>,
    #[serde(default)]
    pub new_skills: bool,
    #[serde(default)]
    pub risky: bool,
}

impl Grant {
    pub fn allows(&self, skill: &str) -> bool {
        self.skills.iter().any(|s| s == skill || s == "*")
    }

    pub fn narrow(&self, other: &Grant) -> Grant {
        let mine = self.skills.iter().any(|s| s == "*");
        let theirs = other.skills.iter().any(|s| s == "*");

        let skills = if mine {
            other.skills.clone()
        } else if theirs {
            self.skills.clone()
        } else {
            self.skills
                .iter()
                .filter(|skill| other.allows(skill))
                .cloned()
                .collect()
        };

        Grant {
            skills,
            new_skills: self.new_skills && other.new_skills,
            risky: self.risky && other.risky,
        }
    }

    pub fn within(&self, ceiling: &Grant) -> bool {
        self.narrow(ceiling) == *self
    }

    pub fn describe(&self) -> String {
        let skills = if self.skills.is_empty() {
            "no skills".to_string()
        } else {
            self.skills.join(", ")
        };

        let mut extras: Vec<&str> = Vec::new();
        if self.new_skills {
            extras.push("may write new skills");
        }
        if self.risky {
            extras.push("may run commands the safety rules stop");
        }

        if extras.is_empty() {
            skills
        } else {
            format!("{}; {}", skills, extras.join("; "))
        }
    }
}

#[cfg(test)]
mod grant_tests {
    use super::*;

    fn grant(skills: &[&str], new_skills: bool, risky: bool) -> Grant {
        Grant {
            skills: skills.iter().map(|s| s.to_string()).collect(),
            new_skills,
            risky,
        }
    }

    #[test]
    fn asking_a_peer_cannot_reach_past_what_the_asker_already_had() {
        let asker = grant(&["searxng_search"], false, false);
        let peer = grant(&["shell_executor", "searxng_search"], true, true);

        let effective = peer.narrow(&asker);

        assert_eq!(effective.skills, vec!["searxng_search"]);
        assert!(
            !effective.skills.iter().any(|s| s == "shell_executor"),
            "an agent laundered its way to the shell through a peer"
        );
        assert!(!effective.new_skills, "writing skills leaked sideways");
        assert!(!effective.risky, "the risky flag leaked sideways");
    }

    #[test]
    fn two_agents_that_share_nothing_end_up_able_to_do_nothing() {
        let a = grant(&["telegram"], false, false);
        let b = grant(&["shell_executor"], false, false);

        assert!(a.narrow(&b).skills.is_empty());
    }

    #[test]
    fn a_wildcard_gives_way_to_the_narrower_side() {
        let open = grant(&["*"], true, true);
        let tight = grant(&["telegram"], false, false);

        assert_eq!(open.narrow(&tight).skills, vec!["telegram"]);
        assert_eq!(tight.narrow(&open).skills, vec!["telegram"]);
    }

    #[test]
    fn narrowing_never_hands_out_a_flag_neither_side_had() {
        let a = grant(&["*"], true, false);
        let b = grant(&["*"], false, true);

        let effective = a.narrow(&b);
        assert!(!effective.new_skills);
        assert!(!effective.risky);
    }

    #[test]
    fn a_request_above_the_ceiling_is_seen_as_above_it() {
        let ceiling = grant(&["*"], false, false);

        assert!(grant(&["shell_executor"], false, false).within(&ceiling));
        assert!(
            !grant(&["shell_executor"], true, false).within(&ceiling),
            "writing new skills slipped past a ceiling that forbids it"
        );
        assert!(
            !grant(&[], false, true).within(&ceiling),
            "running risky commands slipped past a ceiling that forbids it"
        );
    }

    #[test]
    fn a_ceiling_that_names_skills_refuses_one_it_does_not_name() {
        let ceiling = grant(&["telegram", "searxng_search"], false, false);

        assert!(grant(&["telegram"], false, false).within(&ceiling));
        assert!(!grant(&["shell_executor"], false, false).within(&ceiling));
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GroupView {
    pub id: String,
    pub goal: String,
    pub iterations_left: u32,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Origin {
    pub source: String,
    pub who: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskObject {
    pub task_id: String,
    #[serde(skip)]
    pub agent_id: String,
    pub parent_task_id: Option<String>,
    pub message: String,
    pub system_info: SystemInfo,
    pub system_response: Option<String>,
    pub skills: Vec<TaskObjectSkill>,
    pub capabilities: Vec<String>,
    pub constraints: Constraints,
    pub iteration: u32,
    pub fix_iteration: u32,
    pub depth: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grant: Option<Grant>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub origin: Option<Origin>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub intelligence: Option<crate::core::intelligence::Standing>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub group: Option<GroupView>,
    pub interface_mode: InterfaceMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InterfaceMode {
    Cli,
    Voice,
}

impl InterfaceMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            InterfaceMode::Cli => "cli",
            InterfaceMode::Voice => "voice",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Choice {
    #[serde(default, deserialize_with = "flexible_string")]
    pub label: String,
    #[serde(default, deserialize_with = "flexible_string")]
    pub value: String,
}

impl Choice {
    #[cfg(test)]
    pub fn new(label: impl Into<String>, value: impl Into<String>) -> Self {
        Choice {
            label: label.into(),
            value: value.into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ActionType {
    ExecuteModule {
        module: String,
        method: String,
        #[serde(default, deserialize_with = "flexible_string")]
        args: String,
        #[serde(default)]
        parallel: bool,
    },
    #[serde(alias = "Respond", alias = "Answer")]
    RespondToUser,
    #[serde(alias = "RequestPermission", alias = "AskPermission")]
    PermissionRequest {
        #[serde(default, deserialize_with = "flexible_string")]
        action: String,
        #[serde(default, deserialize_with = "flexible_string")]
        description: String,
        #[serde(default, deserialize_with = "flexible_string")]
        risk_level: String,
    },
    #[serde(alias = "PromptUser", alias = "AskUser")]
    PromptToUser {
        #[serde(default, deserialize_with = "flexible_string")]
        message: String,
        #[serde(default)]
        options: Vec<Choice>,
    },
    RequestData {
        #[serde(default, deserialize_with = "flexible_string")]
        source: String,
        #[serde(default, deserialize_with = "flexible_string")]
        query: String,
        #[serde(default = "default_request_limit")]
        limit: u32,
    },
    #[serde(alias = "RequestInboxAccess", alias = "AskForInboxKey")]
    RequestInboxKey {
        #[serde(default, deserialize_with = "flexible_string")]
        module: String,
        #[serde(default, deserialize_with = "flexible_string")]
        why: String,
        #[serde(default, deserialize_with = "flexible_string_vec")]
        skills: Vec<String>,
    },
    #[serde(alias = "Memorise", alias = "Memorize", alias = "SaveFact")]
    Remember {
        #[serde(default, deserialize_with = "flexible_string")]
        subject: String,
        #[serde(default, deserialize_with = "flexible_string")]
        key: String,
        #[serde(default, deserialize_with = "flexible_string")]
        value: String,
        #[serde(default, deserialize_with = "flexible_string")]
        note: String,
        #[serde(default, deserialize_with = "flexible_string")]
        owner: String,
        #[serde(default, deserialize_with = "flexible_string")]
        scope: String,
        #[serde(default, deserialize_with = "flexible_string")]
        scope_ref: String,
        #[serde(default)]
        pinned: bool,
        #[serde(default)]
        also: bool,
    },
    #[serde(alias = "ForgetFact")]
    Forget {
        #[serde(default, deserialize_with = "flexible_string")]
        subject: String,
        #[serde(default, deserialize_with = "flexible_string")]
        key: String,
    },
    #[serde(alias = "CreateJob", alias = "Schedule", alias = "Remind")]
    ScheduleJob {
        #[serde(default, deserialize_with = "flexible_string")]
        name: String,
        #[serde(default, deserialize_with = "flexible_string")]
        task: String,
        #[serde(default, deserialize_with = "flexible_string")]
        schedule: String,
        #[serde(default)]
        grant: Grant,
    },
    #[serde(alias = "StopJob", alias = "ListJobs")]
    ManageJobs {
        #[serde(default, deserialize_with = "flexible_string")]
        operation: String,
        #[serde(default)]
        id: i64,
    },
    #[serde(
        alias = "SetIntelligence",
        alias = "SwitchLevel",
        alias = "SwitchModel"
    )]
    Switch {
        #[serde(default, deserialize_with = "flexible_string")]
        level: String,
        #[serde(default, deserialize_with = "flexible_string")]
        why: String,
    },
    #[serde(alias = "Spawn", alias = "SubAgent", alias = "SpawnSubAgent")]
    SpawnAgent {
        #[serde(default, deserialize_with = "flexible_string")]
        task: String,
        #[serde(default, deserialize_with = "flexible_string")]
        reason: String,
        #[serde(default, deserialize_with = "flexible_string")]
        role: String,
    },
    #[serde(alias = "Board", alias = "WriteToBoard")]
    PostToBoard {
        #[serde(default, deserialize_with = "flexible_string")]
        kind: String,
        #[serde(default, deserialize_with = "flexible_string")]
        to: String,
        #[serde(default, deserialize_with = "flexible_string")]
        body: String,
        #[serde(default)]
        entry: i64,
        #[serde(default, deserialize_with = "flexible_string")]
        state: String,
    },
    #[serde(alias = "Ask", alias = "MessageAgent", alias = "TalkTo")]
    AskAgent {
        #[serde(default, deserialize_with = "flexible_string")]
        to: String,
        #[serde(default, deserialize_with = "flexible_string")]
        message: String,
    },
    #[serde(alias = "Verdict", alias = "AllowOrRefuse")]
    Decide {
        #[serde(default, deserialize_with = "flexible_string")]
        id: String,
        #[serde(default)]
        allow: bool,
        #[serde(default, deserialize_with = "flexible_string")]
        answer: String,
        #[serde(default, deserialize_with = "flexible_string")]
        why: String,
    },
    #[serde(
        alias = "AskForGrant",
        alias = "ExpandGrant",
        alias = "RequestPermissionFor"
    )]
    RequestGrant {
        #[serde(default, deserialize_with = "flexible_string_vec")]
        skills: Vec<String>,
        #[serde(default)]
        new_skills: bool,
        #[serde(default)]
        risky: bool,
        #[serde(default, deserialize_with = "flexible_string")]
        why: String,
        #[serde(default)]
        critical: bool,
    },
    GenerateChunk {
        module_name: String,
        chunk_index: u32,
        total_chunks: u32,
        #[serde(default, deserialize_with = "flexible_string")]
        code_chunk: String,
        #[serde(default, deserialize_with = "flexible_string_vec")]
        dependencies: Vec<String>,
        #[serde(default, deserialize_with = "flexible_string")]
        language: String,
    },
}

impl ActionType {
    pub fn label(&self) -> String {
        match self {
            ActionType::ExecuteModule { module, method, .. } => {
                format!("skill · {}.{}", module, method)
            }
            ActionType::RespondToUser => "answering".to_string(),
            ActionType::PermissionRequest { action, .. } => {
                format!("asking permission · {}", action)
            }
            ActionType::PromptToUser { .. } => "asking the user".to_string(),
            ActionType::RequestData { source, query, .. } => {
                format!("{} · {}", source, query)
            }
            ActionType::RequestInboxKey { module, .. } => format!("inbox key · {}", module),
            ActionType::Remember { subject, key, .. } => {
                format!("remember · {} {}", subject, key)
            }
            ActionType::Forget { subject, .. } => format!("forget · {}", subject),
            ActionType::ScheduleJob { name, .. } => format!("schedule · {}", name),
            ActionType::ManageJobs { operation, .. } => format!("jobs · {}", operation),
            ActionType::Switch { level, .. } => format!("intelligence · {}", level),
            ActionType::SpawnAgent { role, .. } if !role.trim().is_empty() => {
                format!("spawning a {}", role)
            }
            ActionType::SpawnAgent { .. } => "spawning a sub-agent".to_string(),
            ActionType::PostToBoard { kind, entry, .. } if *entry > 0 => {
                format!("board · #{} {}", entry, kind)
            }
            ActionType::PostToBoard { kind, .. } => format!("board · {}", kind),
            ActionType::AskAgent { to, .. } => format!("asking {}", to),
            ActionType::RequestGrant { why, .. } => format!("asking for rights · {}", why),
            ActionType::Decide {
                id, allow, answer, ..
            } => {
                let what = if !answer.trim().is_empty() {
                    "answering"
                } else if *allow {
                    "allowing"
                } else {
                    "refusing"
                };
                format!("{} #{}", what, id)
            }
            ActionType::GenerateChunk {
                module_name,
                chunk_index,
                total_chunks,
                ..
            } => format!(
                "writing {} · chunk {}/{}",
                module_name,
                chunk_index + 1,
                total_chunks
            ),
        }
    }
}

fn default_request_limit() -> u32 {
    5
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentResponse {
    #[serde(default, deserialize_with = "flexible_string")]
    pub message: String,
    #[serde(default)]
    pub is_done: bool,
    #[serde(default)]
    pub actions: Vec<ActionType>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmMessage {
    pub role: String,
    pub content: String,
    #[serde(default, skip)]
    pub stable: bool,
}

impl LlmMessage {
    pub fn new(role: &str, content: impl Into<String>) -> LlmMessage {
        LlmMessage {
            role: role.to_string(),
            content: content.into(),
            stable: false,
        }
    }

    pub fn unchanging(mut self) -> LlmMessage {
        self.stable = true;
        self
    }
}

pub fn agent_response_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "message": { "type": "string" },
            "is_done": { "type": "boolean" },
            "actions": { "type": "array", "items": { "oneOf": action_type_schemas() } }
        },
        "required": ["message", "is_done", "actions"]
    })
}

fn action_type_schemas() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "ExecuteModule" },
                "module": { "type": "string" },
                "method": { "type": "string" },
                "args": { "type": "string" },
                "parallel": { "type": "boolean" }
            },
            "required": ["type", "module", "method", "args"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": { "type": { "const": "RespondToUser" } },
            "required": ["type"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "PermissionRequest" },
                "action": { "type": "string" },
                "description": { "type": "string" },
                "risk_level": { "type": "string" }
            },
            "required": ["type", "action", "description", "risk_level"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "PromptToUser" },
                "message": { "type": "string" },
                "options": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "label": { "type": "string" },
                            "value": { "type": "string" }
                        },
                        "required": ["label", "value"]
                    }
                }
            },
            "required": ["type", "message"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "RequestData" },
                "source": { "type": "string" },
                "query": { "type": "string" },
                "limit": { "type": "integer" }
            },
            "required": ["type", "source", "query"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "RequestInboxKey" },
                "module": { "type": "string" },
                "why": { "type": "string" },
                "skills": { "type": "array", "items": { "type": "string" } }
            },
            "required": ["type", "module", "why"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "Remember" },
                "subject": { "type": "string" },
                "key": { "type": "string" },
                "value": { "type": "string" },
                "note": { "type": "string" }
            },
            "required": ["type", "subject", "key", "value"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "Forget" },
                "subject": { "type": "string" },
                "key": { "type": "string" }
            },
            "required": ["type", "subject"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "ScheduleJob" },
                "name": { "type": "string" },
                "task": { "type": "string" },
                "schedule": { "type": "string" },
                "grant": {
                    "type": "object",
                    "properties": {
                        "skills": { "type": "array", "items": { "type": "string" } },
                        "new_skills": { "type": "boolean" },
                        "risky": { "type": "boolean" }
                    }
                }
            },
            "required": ["type", "name", "task", "schedule"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "ManageJobs" },
                "operation": { "type": "string" },
                "id": { "type": "integer" }
            },
            "required": ["type", "operation"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "Switch" },
                "level": { "type": "string" },
                "why": { "type": "string" }
            },
            "required": ["type", "level"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "SpawnAgent" },
                "task": { "type": "string" },
                "reason": { "type": "string" }
            },
            "required": ["type", "task"]
        }),
        serde_json::json!({
            "type": "object",
            "properties": {
                "type": { "const": "GenerateChunk" },
                "module_name": { "type": "string" },
                "chunk_index": { "type": "integer" },
                "total_chunks": { "type": "integer" },
                "code_chunk": { "type": "string" },
                "dependencies": { "type": "array", "items": { "type": "string" } },
                "language": { "type": "string" }
            },
            "required": ["type", "module_name", "chunk_index", "total_chunks"]
        }),
    ]
}

#[cfg(test)]
mod schema_tests {
    use super::*;

    const CANONICAL_TYPES: [&str; 13] = [
        "ExecuteModule",
        "RespondToUser",
        "PermissionRequest",
        "PromptToUser",
        "RequestData",
        "RequestInboxKey",
        "Remember",
        "Forget",
        "ScheduleJob",
        "ManageJobs",
        "Switch",
        "SpawnAgent",
        "GenerateChunk",
    ];

    #[test]
    fn every_canonical_action_type_has_exactly_one_schema_entry() {
        let schemas = action_type_schemas();
        assert_eq!(schemas.len(), CANONICAL_TYPES.len());

        let named: Vec<&str> = schemas
            .iter()
            .map(|schema| {
                schema["properties"]["type"]["const"]
                    .as_str()
                    .expect("every schema entry names its type")
            })
            .collect();

        for wanted in CANONICAL_TYPES {
            assert_eq!(
                named.iter().filter(|&&n| n == wanted).count(),
                1,
                "{wanted} should have exactly one schema entry"
            );
        }
    }

    #[test]
    fn every_schema_entry_requires_its_own_type_tag() {
        for schema in action_type_schemas() {
            let required = schema["required"]
                .as_array()
                .expect("every schema entry lists required fields");
            assert!(
                required.iter().any(|v| v == "type"),
                "{schema} does not require its own type tag"
            );
        }
    }

    #[test]
    fn sample_payloads_meeting_the_schemas_required_fields_parse() {
        let samples = [
            r#"{"type":"ExecuteModule","module":"shell_executor","method":"run","args":"ls"}"#,
            r#"{"type":"RespondToUser"}"#,
            r#"{"type":"Switch","level":"high"}"#,
            r#"{"type":"Remember","subject":"asiya","key":"likes","value":"tea"}"#,
        ];

        for sample in samples {
            let action: ActionType =
                serde_json::from_str(sample).unwrap_or_else(|e| panic!("{sample}: {e}"));
            let tag = serde_json::to_value(&action).unwrap()["type"]
                .as_str()
                .unwrap()
                .to_string();
            assert!(
                CANONICAL_TYPES.contains(&tag.as_str()),
                "{sample} round-tripped to an unlisted type {tag}"
            );
        }
    }

    #[test]
    fn the_response_schema_names_all_three_top_level_fields() {
        let schema = agent_response_schema();
        let required = schema["required"].as_array().unwrap();
        for field in ["message", "is_done", "actions"] {
            assert!(required.iter().any(|v| v == field), "missing {field}");
        }
    }
}
