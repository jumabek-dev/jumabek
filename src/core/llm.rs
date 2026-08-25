use std::time::Duration;

use reqwest::StatusCode;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};

use crate::configs::Config;
use crate::core::json_repair;
use crate::core::task::{AgentResponse, LlmMessage, agent_response_schema};
use crate::error::{JumabekError, JumabekResult};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
const ANTHROPIC_VERSION: &str = "2023-06-01";
const ANTHROPIC_THINKING_BUDGET: u32 = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Protocol {
    #[default]
    OpenAi,
    Anthropic,
}

impl Protocol {
    pub fn parse(raw: &str) -> Option<Protocol> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "openai" => Some(Protocol::OpenAi),
            "anthropic" | "claude" => Some(Protocol::Anthropic),
            _ => None,
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    max_retries: u32,
    initial_delay_ms: u64,
}

#[derive(Debug, Clone)]
pub struct RequestTarget {
    pub protocol: Protocol,
    pub model: String,
    pub endpoint: String,
    pub api_key: String,
    pub reasoning_effort: String,
    pub structured_output: bool,
    pub max_tokens: u32,
}

impl RequestTarget {
    pub fn global(config: &Config) -> Self {
        let protocol = Protocol::parse(&config.llm.protocol).unwrap_or_default();
        RequestTarget {
            protocol,
            model: config.llm.model.clone(),
            endpoint: chat_endpoint(&config.llm.base_uri, protocol),
            api_key: config.api_key.clone(),
            reasoning_effort: config.llm.reasoning_effort.trim().to_string(),
            structured_output: true,
            max_tokens: config.llm.max_tokens,
        }
    }
}

pub struct LlmReply {
    pub response: AgentResponse,
    pub raw_content: String,
}

impl LlmClient {
    pub fn new(config: &Config) -> JumabekResult<Self> {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(config.llm.request_timeout_sec))
            .connect_timeout(CONNECT_TIMEOUT)
            .build()
            .map_err(|e| JumabekError::InternalError(format!("cannot build http client: {}", e)))?;

        Ok(LlmClient {
            http,
            max_retries: config.llm.retry_max_retries.max(1),
            initial_delay_ms: config.llm.retry_initial_delay_ms,
        })
    }

    pub async fn ask_as(
        &self,
        messages: &[LlmMessage],
        target: &RequestTarget,
    ) -> JumabekResult<LlmReply> {
        let content = self.request_content(messages, target).await?;
        let response = parse_agent_response(&content)?;
        Ok(LlmReply {
            response,
            raw_content: json_repair::extract_json_payload(&content),
        })
    }

    pub async fn complete(
        &self,
        system: &str,
        user: &str,
        target: &RequestTarget,
    ) -> JumabekResult<String> {
        let messages = vec![
            LlmMessage {
                role: "system".to_string(),
                content: system.to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: user.to_string(),
            },
        ];
        self.request_content(&messages, target).await
    }

    async fn request_content(
        &self,
        messages: &[LlmMessage],
        target: &RequestTarget,
    ) -> JumabekResult<String> {
        let body = match target.protocol {
            Protocol::OpenAi => build_openai_body(messages, target),
            Protocol::Anthropic => build_anthropic_body(messages, target),
        };

        let mut last_error = JumabekError::LlmUnavailable("no attempt was made".to_string());

        for attempt in 0..self.max_retries {
            match self.attempt(&body, target).await {
                Ok(content) => return Ok(content),
                Err(AttemptError::Fatal(e)) => return Err(e),
                Err(AttemptError::Retryable(e)) => last_error = e,
            }

            if attempt + 1 < self.max_retries {
                let delay = Duration::from_millis(self.initial_delay_ms * (attempt as u64 + 1));
                tokio::time::sleep(delay).await;
            }
        }

        Err(last_error)
    }

    async fn attempt(
        &self,
        body: &serde_json::Value,
        target: &RequestTarget,
    ) -> Result<String, AttemptError> {
        let mut request = self
            .http
            .post(&target.endpoint)
            .header(CONTENT_TYPE, "application/json");

        request = match target.protocol {
            Protocol::OpenAi => {
                if target.api_key.is_empty() {
                    request
                } else {
                    request.header(AUTHORIZATION, format!("Bearer {}", target.api_key))
                }
            }
            Protocol::Anthropic => {
                let key = if target.api_key.is_empty() {
                    "ollama"
                } else {
                    target.api_key.as_str()
                };
                request
                    .header("x-api-key", key)
                    .header("anthropic-version", ANTHROPIC_VERSION)
            }
        };

        let response = request.json(body).send().await.map_err(|e| {
            if e.is_timeout() {
                AttemptError::Retryable(JumabekError::LlmTimeout(e.to_string()))
            } else {
                AttemptError::Retryable(JumabekError::LlmUnavailable(format!(
                    "{} — nothing is answering at {}. Check that the endpoint is running \
                         and that [llm].base_uri points at it.",
                    e, target.endpoint
                )))
            }
        })?;

        let status = response.status();
        let text = response.text().await.map_err(|e| {
            AttemptError::Retryable(JumabekError::LlmUnavailable(format!(
                "cannot read response body: {}",
                e
            )))
        })?;

        if !status.is_success() {
            return Err(classify_status(status, &text));
        }

        extract_content(&text, target.protocol).map_err(AttemptError::Fatal)
    }
}

fn build_openai_body(messages: &[LlmMessage], target: &RequestTarget) -> serde_json::Value {
    let mut body = serde_json::json!({
        "model": target.model,
        "messages": messages,
        "stream": false,
    });

    if !target.reasoning_effort.is_empty() {
        body["reasoning_effort"] = serde_json::Value::String(target.reasoning_effort.clone());
    }

    if target.structured_output {
        body["response_format"] = serde_json::json!({
            "type": "json_schema",
            "json_schema": {
                "name": "agent_response",
                "schema": agent_response_schema(),
                "strict": true
            }
        });
    }

    body
}

fn build_anthropic_body(messages: &[LlmMessage], target: &RequestTarget) -> serde_json::Value {
    let system: Vec<&str> = messages
        .iter()
        .filter(|m| m.role == "system")
        .map(|m| m.content.as_str())
        .collect();
    let rest: Vec<&LlmMessage> = messages.iter().filter(|m| m.role != "system").collect();

    let mut body = serde_json::json!({
        "model": target.model,
        "messages": rest,
        "max_tokens": target.max_tokens,
        "stream": false,
    });

    if !system.is_empty() {
        body["system"] = serde_json::Value::String(system.join("\n\n"));
    }

    match target.reasoning_effort.as_str() {
        "" => {}
        "none" => body["thinking"] = serde_json::json!({ "type": "disabled" }),
        _ => {
            body["thinking"] = serde_json::json!({
                "type": "enabled",
                "budget_tokens": ANTHROPIC_THINKING_BUDGET
            })
        }
    }

    if target.structured_output {
        body["tools"] = serde_json::json!([{
            "name": "agent_response",
            "input_schema": agent_response_schema()
        }]);
        body["tool_choice"] = serde_json::json!({ "type": "tool", "name": "agent_response" });
    }

    body
}

enum AttemptError {
    Retryable(JumabekError),
    Fatal(JumabekError),
}

fn api_root(base_uri: &str) -> String {
    let mut base = base_uri.trim().trim_end_matches('/');

    for suffix in ["/chat/completions", "/messages", "/api/chat", "/chat"] {
        base = base.trim_end_matches(suffix);
    }

    format!("{}/v1", base.trim_end_matches("/v1"))
}

pub fn chat_endpoint(base_uri: &str, protocol: Protocol) -> String {
    let root = api_root(base_uri);
    match protocol {
        Protocol::OpenAi => format!("{}/chat/completions", root),
        Protocol::Anthropic => format!("{}/messages", root),
    }
}

pub fn models_endpoint(base_uri: &str) -> String {
    format!("{}/models", api_root(base_uri))
}

fn classify_status(status: StatusCode, body: &str) -> AttemptError {
    let detail = summarise_error_body(body);

    match status {
        StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
            AttemptError::Fatal(JumabekError::LlmUnavailable(format!(
                "{} — check the API key (JUMABEK_API_KEY or secrets.toml): {}",
                status, detail
            )))
        }
        StatusCode::NOT_FOUND => AttemptError::Fatal(JumabekError::LlmUnavailable(format!(
            "{} — wrong base_uri or model: {}",
            status, detail
        ))),
        StatusCode::TOO_MANY_REQUESTS => AttemptError::Retryable(JumabekError::LlmUnavailable(
            format!("{} — rate limited: {}", status, detail),
        )),
        s if s.is_server_error() => AttemptError::Retryable(JumabekError::LlmUnavailable(format!(
            "{} — provider error: {}",
            status, detail
        ))),
        _ => AttemptError::Fatal(JumabekError::LlmUnavailable(format!(
            "{}: {}",
            status, detail
        ))),
    }
}

fn summarise_error_body(body: &str) -> String {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return "<empty body>".to_string();
    }

    if let Ok(value) = serde_json::from_str::<serde_json::Value>(trimmed) {
        for path in [["error", "message"], ["message", ""], ["detail", ""]] {
            let found = if path[1].is_empty() {
                value.get(path[0]).and_then(|v| v.as_str())
            } else {
                value
                    .get(path[0])
                    .and_then(|v| v.get(path[1]))
                    .and_then(|v| v.as_str())
            };
            if let Some(text) = found {
                return text.to_string();
            }
        }
    }

    trimmed.chars().take(300).collect()
}

fn extract_content(body: &str, protocol: Protocol) -> JumabekResult<String> {
    let raw: serde_json::Value = serde_json::from_str(body).map_err(|e| {
        JumabekError::LlmInvalidResponse(format!(
            "provider returned non-JSON: {} — body starts with: {}",
            e,
            body.chars().take(200).collect::<String>()
        ))
    })?;

    match protocol {
        Protocol::OpenAi => extract_openai_content(&raw, body),
        Protocol::Anthropic => extract_anthropic_content(&raw, body),
    }
}

fn extract_openai_content(raw: &serde_json::Value, body: &str) -> JumabekResult<String> {
    let choices = raw.get("choices").and_then(|c| c.as_array());
    let Some(choices) = choices else {
        return Err(JumabekError::LlmInvalidResponse(format!(
            "response has no 'choices' array: {}",
            body.chars().take(300).collect::<String>()
        )));
    };

    let Some(first) = choices.first() else {
        return Err(JumabekError::LlmInvalidResponse(
            "response contains an empty 'choices' array".to_string(),
        ));
    };

    let content = first
        .get("message")
        .and_then(|m| m.get("content"))
        .and_then(|c| c.as_str())
        .unwrap_or("");

    if content.trim().is_empty() {
        let finish_reason = first
            .get("finish_reason")
            .and_then(|f| f.as_str())
            .unwrap_or("unknown");
        return Err(JumabekError::LlmInvalidResponse(format!(
            "model returned empty content (finish_reason: {})",
            finish_reason
        )));
    }

    Ok(content.to_string())
}

fn extract_anthropic_content(raw: &serde_json::Value, body: &str) -> JumabekResult<String> {
    let blocks = raw.get("content").and_then(|c| c.as_array());
    let Some(blocks) = blocks else {
        return Err(JumabekError::LlmInvalidResponse(format!(
            "response has no 'content' array: {}",
            body.chars().take(300).collect::<String>()
        )));
    };

    let tool_use = blocks
        .iter()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("tool_use"));

    if let Some(block) = tool_use {
        let input = block
            .get("input")
            .cloned()
            .unwrap_or(serde_json::Value::Null);
        return serde_json::to_string(&input).map_err(|e| {
            JumabekError::LlmInvalidResponse(format!("cannot encode tool_use input: {}", e))
        });
    }

    let text = blocks
        .iter()
        .find(|b| b.get("type").and_then(|t| t.as_str()) == Some("text"))
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        .unwrap_or("");

    if text.trim().is_empty() {
        let stop_reason = raw
            .get("stop_reason")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown");
        return Err(JumabekError::LlmInvalidResponse(format!(
            "model returned empty content (stop_reason: {})",
            stop_reason
        )));
    }

    Ok(text.to_string())
}

pub fn parse_agent_response(content: &str) -> JumabekResult<AgentResponse> {
    let payload = json_repair::extract_json_payload(content);

    serde_json::from_str::<AgentResponse>(&payload).map_err(|e| {
        if json_repair::looks_truncated(content) {
            return JumabekError::ParseError(format!(
                "response looks truncated (unclosed JSON), the model probably hit its output limit: {}",
                e
            ));
        }

        JumabekError::ParseError(format!(
            "cannot read the answer as an agent response: {} — got: {}",
            e,
            payload.chars().take(400).collect::<String>()
        ))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::task::ActionType;

    #[test]
    fn every_way_a_server_documents_its_address_lands_on_one_openai_endpoint() {
        assert_eq!(
            chat_endpoint("http://localhost:20128/api", Protocol::OpenAi),
            "http://localhost:20128/api/v1/chat/completions"
        );
        assert_eq!(
            chat_endpoint("http://localhost:11434/v1", Protocol::OpenAi),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            chat_endpoint("http://localhost:1234/v1/", Protocol::OpenAi),
            "http://localhost:1234/v1/chat/completions"
        );
        assert_eq!(
            chat_endpoint("http://localhost:11434", Protocol::OpenAi),
            "http://localhost:11434/v1/chat/completions"
        );
        assert_eq!(
            chat_endpoint(
                "https://api.example.com/v1/chat/completions",
                Protocol::OpenAi
            ),
            "https://api.example.com/v1/chat/completions"
        );
    }

    #[test]
    fn anthropic_protocol_lands_on_the_messages_endpoint() {
        assert_eq!(
            chat_endpoint("http://localhost:11434", Protocol::Anthropic),
            "http://localhost:11434/v1/messages"
        );
        assert_eq!(
            chat_endpoint("http://localhost:11434/v1/messages", Protocol::Anthropic),
            "http://localhost:11434/v1/messages"
        );
    }

    #[test]
    fn protocol_parses_the_names_a_config_might_use() {
        assert_eq!(Protocol::parse(""), Some(Protocol::OpenAi));
        assert_eq!(Protocol::parse("openai"), Some(Protocol::OpenAi));
        assert_eq!(Protocol::parse("Anthropic"), Some(Protocol::Anthropic));
        assert_eq!(Protocol::parse("claude"), Some(Protocol::Anthropic));
        assert_eq!(Protocol::parse("ollama-native"), None);
    }

    #[test]
    fn doctor_probes_the_same_place_the_agent_posts_to() {
        for base in [
            "http://localhost:20128/api",
            "http://localhost:11434/v1",
            "http://localhost:11434",
            "https://api.example.com/v1/chat/completions",
        ] {
            let chat = chat_endpoint(base, Protocol::OpenAi);
            let models = models_endpoint(base);

            assert_eq!(
                chat.trim_end_matches("/chat/completions"),
                models.trim_end_matches("/models"),
                "doctor would report an endpoint the agent never talks to, for {base}"
            );
        }
    }

    fn body_with(content: &str) -> String {
        serde_json::json!({
            "choices": [{ "message": { "role": "assistant", "content": content } }]
        })
        .to_string()
    }

    #[test]
    fn reads_content_out_of_envelope() {
        let body = body_with(r#"{"message":"ok","is_done":true,"actions":[]}"#);
        assert!(
            extract_content(&body, Protocol::OpenAi)
                .unwrap()
                .contains("\"ok\"")
        );
    }

    #[test]
    fn rejects_error_envelope_instead_of_returning_empty() {
        let body = r#"{"error":{"message":"invalid api key","type":"auth_error"}}"#;
        let err = extract_content(body, Protocol::OpenAi).unwrap_err();
        assert!(matches!(err, JumabekError::LlmInvalidResponse(_)));
    }

    #[test]
    fn reports_empty_content_with_finish_reason() {
        let body = serde_json::json!({
            "choices": [{ "message": { "content": "" }, "finish_reason": "length" }]
        })
        .to_string();
        let err = extract_content(&body, Protocol::OpenAi)
            .unwrap_err()
            .to_string();
        assert!(err.contains("length"), "got: {err}");
    }

    #[test]
    fn anthropic_content_prefers_a_tool_use_block() {
        let body = serde_json::json!({
            "content": [
                { "type": "text", "text": "ignored" },
                { "type": "tool_use", "name": "agent_response", "input": {"message": "hi", "is_done": true, "actions": []} }
            ]
        })
        .to_string();
        let content = extract_content(&body, Protocol::Anthropic).unwrap();
        assert!(content.contains("\"is_done\":true"));
    }

    #[test]
    fn anthropic_content_falls_back_to_a_text_block() {
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "{\"message\":\"hi\"}" }]
        })
        .to_string();
        assert_eq!(
            extract_content(&body, Protocol::Anthropic).unwrap(),
            r#"{"message":"hi"}"#
        );
    }

    #[test]
    fn anthropic_reports_empty_content_with_stop_reason() {
        let body = serde_json::json!({
            "content": [{ "type": "text", "text": "" }],
            "stop_reason": "max_tokens"
        })
        .to_string();
        let err = extract_content(&body, Protocol::Anthropic)
            .unwrap_err()
            .to_string();
        assert!(err.contains("max_tokens"), "got: {err}");
    }

    fn target(protocol: Protocol) -> RequestTarget {
        RequestTarget {
            protocol,
            model: "test-model".to_string(),
            endpoint: "http://localhost/x".to_string(),
            api_key: String::new(),
            reasoning_effort: String::new(),
            structured_output: false,
            max_tokens: 8192,
        }
    }

    #[test]
    fn anthropic_body_moves_system_out_of_messages_and_always_sets_max_tokens() {
        let messages = vec![
            LlmMessage {
                role: "system".to_string(),
                content: "you are jumabek".to_string(),
            },
            LlmMessage {
                role: "user".to_string(),
                content: "hi".to_string(),
            },
        ];

        let body = build_anthropic_body(&messages, &target(Protocol::Anthropic));

        assert_eq!(body["system"], "you are jumabek");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn anthropic_structured_output_forces_the_agent_response_tool() {
        let mut t = target(Protocol::Anthropic);
        t.structured_output = true;
        let body = build_anthropic_body(&[], &t);

        assert_eq!(body["tool_choice"]["name"], "agent_response");
        assert_eq!(body["tools"][0]["name"], "agent_response");
    }

    #[test]
    fn summarises_provider_error_message() {
        assert_eq!(
            summarise_error_body(r#"{"error":{"message":"model not found"}}"#),
            "model not found"
        );
        assert_eq!(summarise_error_body("   "), "<empty body>");
    }

    #[test]
    fn auth_failure_is_fatal_but_rate_limit_retries() {
        assert!(matches!(
            classify_status(StatusCode::UNAUTHORIZED, "{}"),
            AttemptError::Fatal(_)
        ));
        assert!(matches!(
            classify_status(StatusCode::TOO_MANY_REQUESTS, "{}"),
            AttemptError::Retryable(_)
        ));
        assert!(matches!(
            classify_status(StatusCode::INTERNAL_SERVER_ERROR, "{}"),
            AttemptError::Retryable(_)
        ));
    }

    #[test]
    fn parses_response_wrapped_in_markdown() {
        let content = "```json\n{\"message\":\"готово\",\"is_done\":true,\"actions\":[]}\n```";
        let parsed = parse_agent_response(content).unwrap();
        assert_eq!(parsed.message, "готово");
        assert!(parsed.is_done);
    }

    #[test]
    fn fills_missing_fields_with_defaults() {
        let parsed = parse_agent_response(r#"{"message":"hi"}"#).unwrap();
        assert!(!parsed.is_done);
        assert!(parsed.actions.is_empty());
    }

    #[test]
    fn accepts_action_aliases() {
        let content = r#"{"message":"","actions":[
            {"type":"PromptUser","message":"which one?","options":[]},
            {"type":"Respond"}
        ]}"#;
        let parsed = parse_agent_response(content).unwrap();
        assert!(matches!(parsed.actions[0], ActionType::PromptToUser { .. }));
        assert!(matches!(parsed.actions[1], ActionType::RespondToUser));
    }

    #[test]
    fn spawn_agent_is_recognised_under_its_aliases() {
        for name in ["SpawnAgent", "Spawn", "SubAgent", "SpawnSubAgent"] {
            let content = format!(
                r#"{{"actions":[{{"type":"{}","task":"read the logs","reason":"long"}}]}}"#,
                name
            );
            let parsed = parse_agent_response(&content).unwrap();
            match &parsed.actions[0] {
                ActionType::SpawnAgent { task, reason } => {
                    assert_eq!(task, "read the logs");
                    assert_eq!(reason, "long");
                }
                other => panic!("{} did not parse as a spawn: {:?}", name, other),
            }
        }
    }

    #[test]
    fn request_data_limit_defaults() {
        let content = r#"{"actions":[{"type":"RequestData","source":"memory","query":"doc"}]}"#;
        let parsed = parse_agent_response(content).unwrap();
        match &parsed.actions[0] {
            ActionType::RequestData { limit, .. } => assert_eq!(*limit, 5),
            other => panic!("unexpected action: {:?}", other),
        }
    }

    #[test]
    fn coerces_non_string_fields_the_model_gets_wrong() {
        let content = r#"{"message":"ok","actions":[
            {"type":"ExecuteModule","module":"slowpoke","method":"sleep","args":1},
            {"type":"ExecuteModule","module":"shell","method":"run","args":true},
            {"type":"ExecuteModule","module":"shell","method":"run","args":{"path":"/tmp"}}
        ]}"#;
        let parsed = parse_agent_response(content).unwrap();

        let args: Vec<&str> = parsed
            .actions
            .iter()
            .map(|a| match a {
                ActionType::ExecuteModule { args, .. } => args.as_str(),
                _ => panic!("unexpected action"),
            })
            .collect();

        assert_eq!(args, vec!["1", "true", r#"{"path":"/tmp"}"#]);
    }

    #[test]
    fn odd_shapes_in_lists_do_not_kill_the_turn() {
        let content = r#"{"actions":[
            {"type":"PermissionRequest","action":"x","description":"y","risk_level":"low",
             "options":[{"label":"Allow","value":"allow"}]},
            {"type":"GenerateChunk","module_name":"m","chunk_index":1,"total_chunks":1,
             "code_chunk":"fn main(){}","dependencies":[{"name":"regex","version":"1"}]}
        ]}"#;
        let parsed = parse_agent_response(content).unwrap();
        assert_eq!(parsed.actions.len(), 2);
    }

    #[test]
    fn a_numeric_message_does_not_kill_the_turn() {
        let parsed = parse_agent_response(r#"{"message":42,"is_done":true}"#).unwrap();
        assert_eq!(parsed.message, "42");
    }

    #[test]
    fn truncated_response_says_so() {
        let err = parse_agent_response(r#"{"message":"half a sen"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("truncated"), "got: {err}");
    }

    #[test]
    fn unknown_action_type_names_the_variants() {
        let err = parse_agent_response(r#"{"actions":[{"type":"Teleport"}]}"#)
            .unwrap_err()
            .to_string();
        assert!(err.contains("ExecuteModule"), "got: {err}");
    }
}
