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

    pub fn id(self) -> &'static str {
        match self {
            Protocol::OpenAi => "openai",
            Protocol::Anthropic => "anthropic",
        }
    }
}

#[derive(Clone)]
pub struct LlmClient {
    http: reqwest::Client,
    max_retries: u32,
    initial_delay_ms: u64,
    cache_markers: std::sync::Arc<std::sync::atomic::AtomicBool>,
    streaming: std::sync::Arc<std::sync::atomic::AtomicBool>,
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
    pub usage: Option<crate::core::usage::Usage>,
}

pub struct Answer {
    pub content: String,
    pub usage: Option<crate::core::usage::Usage>,
}

pub type Watcher = tokio::sync::mpsc::UnboundedSender<String>;

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
            cache_markers: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
            streaming: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        })
    }

    pub async fn ask_as(
        &self,
        messages: &[LlmMessage],
        target: &RequestTarget,
    ) -> JumabekResult<LlmReply> {
        self.ask_watched(messages, target, None).await
    }

    pub async fn ask_watched(
        &self,
        messages: &[LlmMessage],
        target: &RequestTarget,
        watcher: Option<Watcher>,
    ) -> JumabekResult<LlmReply> {
        let answer = self.request(messages, target, watcher).await?;
        let response = parse_agent_response(&answer.content)?;
        Ok(LlmReply {
            response,
            raw_content: json_repair::extract_json_payload(&answer.content),
            usage: answer.usage,
        })
    }

    pub async fn complete(
        &self,
        system: &str,
        user: &str,
        target: &RequestTarget,
    ) -> JumabekResult<String> {
        let messages = vec![
            LlmMessage::new("system", system),
            LlmMessage::new("user", user),
        ];
        Ok(self.request(&messages, target, None).await?.content)
    }

    async fn request(
        &self,
        messages: &[LlmMessage],
        target: &RequestTarget,
        watcher: Option<Watcher>,
    ) -> JumabekResult<Answer> {
        let mut marking = self
            .cache_markers
            .load(std::sync::atomic::Ordering::Relaxed);
        let mut flowing = self.streaming.load(std::sync::atomic::Ordering::Relaxed);
        let mut body = self.body_for(messages, target, marking, flowing);

        let mut last_error = JumabekError::LlmUnavailable("no attempt was made".to_string());

        for attempt in 0..self.max_retries {
            let outcome = if flowing {
                self.attempt_streaming(&body, target, watcher.as_ref())
                    .await
            } else {
                self.attempt(&body, target).await
            };

            match outcome {
                Ok(answer) => return Ok(answer),
                Err(AttemptError::Fatal(e)) if marking && refused_the_marker(&e) => {
                    self.cache_markers
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    marking = false;
                    body = self.body_for(messages, target, false, flowing);
                    last_error = e;
                    continue;
                }
                Err(AttemptError::Fatal(e)) if flowing && refused_the_flow(&e) => {
                    self.streaming
                        .store(false, std::sync::atomic::Ordering::Relaxed);
                    flowing = false;
                    body = self.body_for(messages, target, marking, false);
                    last_error = e;
                    continue;
                }
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

    fn body_for(
        &self,
        messages: &[LlmMessage],
        target: &RequestTarget,
        mark_cache: bool,
        flowing: bool,
    ) -> serde_json::Value {
        let mut body = match target.protocol {
            Protocol::OpenAi => build_openai_body(messages, target, mark_cache),
            Protocol::Anthropic => build_anthropic_body(messages, target, mark_cache),
        };
        body["stream"] = serde_json::Value::Bool(flowing);
        body
    }

    async fn attempt_streaming(
        &self,
        body: &serde_json::Value,
        target: &RequestTarget,
        watcher: Option<&Watcher>,
    ) -> Result<Answer, AttemptError> {
        use futures::StreamExt;

        let response = self.send(body, target).await?;
        let status = response.status();

        if !status.is_success() {
            let text = response.text().await.unwrap_or_default();
            return Err(classify_status(status, &text));
        }

        let mut assembler = crate::core::stream::Assembler::new(target.protocol);
        let mut chunks = response.bytes_stream();
        let mut whole_body = String::new();

        while let Some(chunk) = chunks.next().await {
            let chunk = chunk.map_err(|e| {
                AttemptError::Retryable(JumabekError::LlmUnavailable(format!(
                    "the answer stopped part way through: {}",
                    e
                )))
            })?;

            let text = String::from_utf8_lossy(&chunk).to_string();
            whole_body.push_str(&text);

            if let Some(fresh) = assembler.feed(&text)
                && let Some(watcher) = watcher
            {
                let _ = watcher.send(fresh);
            }
        }

        // Some endpoints accept `stream: true` and answer with an ordinary body
        // anyway. That is a whole answer, not a broken one, so read it as one.
        if !assembler.saw_any_events() {
            let content =
                extract_content(&whole_body, target.protocol).map_err(AttemptError::Retryable)?;

            if let Some(watcher) = watcher
                && let Some(message) = crate::core::stream::visible_message(&content)
            {
                let _ = watcher.send(message);
            }

            return Ok(Answer {
                content,
                usage: crate::core::usage::parse(&whole_body),
            });
        }

        // A stream that stopped without its terminator was cut short, and what
        // arrived is half an answer. Asking again is better than parsing it.
        let finished = assembler.saw_the_end();
        let (content, usage) = assembler.finish();

        if content.trim().is_empty() {
            return Err(AttemptError::Retryable(JumabekError::LlmInvalidResponse(
                "the model streamed nothing back".to_string(),
            )));
        }

        if !finished {
            return Err(AttemptError::Retryable(JumabekError::LlmInvalidResponse(
                format!(
                    "the answer was cut off after {} characters with no end marker",
                    content.chars().count()
                ),
            )));
        }

        Ok(Answer { content, usage })
    }

    async fn send(
        &self,
        body: &serde_json::Value,
        target: &RequestTarget,
    ) -> Result<reqwest::Response, AttemptError> {
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

        request.json(body).send().await.map_err(|e| {
            if e.is_timeout() {
                AttemptError::Retryable(JumabekError::LlmTimeout(e.to_string()))
            } else {
                AttemptError::Retryable(JumabekError::LlmUnavailable(format!(
                    "{} — nothing is answering at {}. Check that the endpoint is running \
                     and that [llm].base_uri points at it.",
                    e, target.endpoint
                )))
            }
        })
    }

    async fn attempt(
        &self,
        body: &serde_json::Value,
        target: &RequestTarget,
    ) -> Result<Answer, AttemptError> {
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

        let usage = crate::core::usage::parse(&text);
        let content = extract_content(&text, target.protocol).map_err(AttemptError::Fatal)?;

        Ok(Answer { content, usage })
    }
}

fn last_stable(messages: &[LlmMessage]) -> Option<usize> {
    messages.iter().rposition(|m| m.stable)
}

fn build_openai_body(
    messages: &[LlmMessage],
    target: &RequestTarget,
    mark_cache: bool,
) -> serde_json::Value {
    let breakpoint = if mark_cache {
        last_stable(messages)
    } else {
        None
    };

    let wire: Vec<serde_json::Value> = messages
        .iter()
        .enumerate()
        .map(|(at, m)| {
            if Some(at) == breakpoint {
                serde_json::json!({
                    "role": m.role,
                    "content": [{
                        "type": "text",
                        "text": m.content,
                        "cache_control": { "type": "ephemeral" }
                    }]
                })
            } else {
                serde_json::json!({ "role": m.role, "content": m.content })
            }
        })
        .collect();

    let mut body = serde_json::json!({
        "model": target.model,
        "messages": wire,
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

fn build_anthropic_body(
    messages: &[LlmMessage],
    target: &RequestTarget,
    mark_cache: bool,
) -> serde_json::Value {
    let system: Vec<&LlmMessage> = messages.iter().filter(|m| m.role == "system").collect();
    let cut = if mark_cache {
        system.iter().rposition(|m| m.stable)
    } else {
        None
    };
    let rest: Vec<serde_json::Value> = messages
        .iter()
        .filter(|m| m.role != "system")
        .map(|m| serde_json::json!({ "role": m.role, "content": m.content }))
        .collect();

    let mut body = serde_json::json!({
        "model": target.model,
        "messages": rest,
        "max_tokens": target.max_tokens,
        "stream": false,
    });

    if !system.is_empty() {
        body["system"] = serde_json::Value::Array(
            system
                .iter()
                .enumerate()
                .map(|(at, m)| {
                    let mut block = serde_json::json!({ "type": "text", "text": m.content });
                    if Some(at) == cut {
                        block["cache_control"] = serde_json::json!({ "type": "ephemeral" });
                    }
                    block
                })
                .collect(),
        );
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

fn refused_the_flow(error: &JumabekError) -> bool {
    error.to_string().to_lowercase().contains("stream")
}

fn refused_the_marker(error: &JumabekError) -> bool {
    let text = error.to_string().to_lowercase();
    text.contains("cache_control")
        || text.contains("content")
            && (text.contains("400") || text.contains("invalid") || text.contains("unsupported"))
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

    if !content.trim().is_empty() {
        return Ok(content.to_string());
    }

    // Some models (gpt-oss on Ollama, notably) answer a json_schema request through the
    // tool_calls channel instead of content, sometimes with a tool that was never offered.
    // Hand whatever arguments came back to the same parser content normally goes through —
    // it either matches the schema, or it doesn't and the existing unreadable-answer retry
    // handles it, instead of this failing outright on every such turn.
    let tool_arguments = first
        .get("message")
        .and_then(|m| m.get("tool_calls"))
        .and_then(|calls| calls.as_array())
        .and_then(|calls| calls.first())
        .and_then(|call| call.get("function"))
        .and_then(|f| f.get("arguments"))
        .and_then(|a| a.as_str());

    if let Some(arguments) = tool_arguments
        && !arguments.trim().is_empty()
    {
        return Ok(arguments.to_string());
    }

    let finish_reason = first
        .get("finish_reason")
        .and_then(|f| f.as_str())
        .unwrap_or("unknown");
    Err(JumabekError::LlmInvalidResponse(format!(
        "model returned empty content (finish_reason: {})",
        finish_reason
    )))
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

    if let Ok(response) = serde_json::from_str::<AgentResponse>(&payload) {
        return Ok(response);
    }

    // A model writing markdown into a JSON string leaves backslashes JSON cannot
    // read. Try again with those escaped rather than spending a turn asking.
    let mended = json_repair::repair_escapes(&payload);
    if mended != payload
        && let Ok(response) = serde_json::from_str::<AgentResponse>(&mended)
    {
        return Ok(response);
    }

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
    fn falls_back_to_tool_calls_when_content_is_empty() {
        let body = serde_json::json!({
            "choices": [{
                "message": {
                    "content": "",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {
                            "name": "container.exec",
                            "arguments": "{\"message\":\"hi\",\"is_done\":true,\"actions\":[]}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }]
        })
        .to_string();

        let content = extract_content(&body, Protocol::OpenAi).unwrap();
        assert_eq!(content, r#"{"message":"hi","is_done":true,"actions":[]}"#);
    }

    #[test]
    fn empty_tool_calls_still_reports_finish_reason() {
        let body = serde_json::json!({
            "choices": [{ "message": { "content": "", "tool_calls": [] }, "finish_reason": "tool_calls" }]
        })
        .to_string();

        let err = extract_content(&body, Protocol::OpenAi)
            .unwrap_err()
            .to_string();
        assert!(err.contains("tool_calls"), "got: {err}");
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
            LlmMessage::new("system", "you are jumabek"),
            LlmMessage::new("user", "hi"),
        ];

        let body = build_anthropic_body(&messages, &target(Protocol::Anthropic), false);

        assert_eq!(body["system"][0]["text"], "you are jumabek");
        assert_eq!(body["max_tokens"], 8192);
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    fn turn(volatile: &str) -> Vec<LlmMessage> {
        vec![
            LlmMessage::new("system", "the standing instructions").unchanging(),
            LlmMessage::new("system", "pinned: the user is called Aibar").unchanging(),
            LlmMessage::new("system", format!("fetched for this turn: {}", volatile)),
            LlmMessage::new("user", format!("please do {}", volatile)),
        ]
    }

    #[test]
    fn the_cache_is_cut_after_the_last_stable_block_on_the_anthropic_path() {
        let body = build_anthropic_body(&turn("a"), &target(Protocol::Anthropic), true);
        let system = body["system"].as_array().expect("system should be blocks");

        assert_eq!(system.len(), 3);
        assert!(system[0].get("cache_control").is_none());
        assert_eq!(system[1]["cache_control"]["type"], "ephemeral");
        assert!(
            system[2].get("cache_control").is_none(),
            "the marker sat after the part that changes every turn, so it can never hit"
        );
    }

    #[test]
    fn the_cache_is_cut_after_the_last_stable_message_on_the_openai_path() {
        let body = build_openai_body(&turn("a"), &target(Protocol::OpenAi), true);
        let sent = body["messages"].as_array().unwrap();

        assert!(sent[0]["content"].is_string());
        assert_eq!(sent[1]["content"][0]["cache_control"]["type"], "ephemeral");
        assert!(
            sent[2]["content"].is_string(),
            "a message that changes every turn was wrapped as if it were cacheable"
        );
    }

    #[test]
    fn the_cached_prefix_is_byte_identical_between_turns_that_share_it() {
        let first = build_anthropic_body(&turn("one thing"), &target(Protocol::Anthropic), true);
        let second = build_anthropic_body(&turn("another"), &target(Protocol::Anthropic), true);

        let prefix = |body: &serde_json::Value| {
            let blocks = body["system"].as_array().unwrap();
            let cut = blocks
                .iter()
                .position(|b| b.get("cache_control").is_some())
                .expect("nothing was marked");
            serde_json::to_string(&blocks[..=cut]).unwrap()
        };

        assert_eq!(
            prefix(&first),
            prefix(&second),
            "the cached prefix changed between turns, so every turn is a cache miss"
        );
        assert_ne!(first["system"], second["system"], "the test proved nothing");
    }

    #[test]
    fn the_openai_cached_prefix_is_byte_identical_between_turns_too() {
        let prefix = |volatile: &str| {
            let body = build_openai_body(&turn(volatile), &target(Protocol::OpenAi), true);
            let sent = body["messages"].as_array().unwrap().clone();
            let cut = sent
                .iter()
                .position(|m| {
                    m["content"]
                        .get(0)
                        .and_then(|c| c.get("cache_control"))
                        .is_some()
                })
                .expect("nothing was marked");
            serde_json::to_string(&sent[..=cut]).unwrap()
        };

        assert_eq!(prefix("one thing"), prefix("another"));
    }

    #[test]
    fn nothing_is_marked_when_the_endpoint_has_already_refused_the_marker() {
        let body = build_anthropic_body(&turn("a"), &target(Protocol::Anthropic), false);
        assert!(
            body["system"]
                .as_array()
                .unwrap()
                .iter()
                .all(|b| b.get("cache_control").is_none())
        );

        let plain = build_openai_body(&turn("a"), &target(Protocol::OpenAi), false);
        assert!(
            plain["messages"]
                .as_array()
                .unwrap()
                .iter()
                .all(|m| m["content"].is_string())
        );
    }

    #[test]
    fn a_turn_with_nothing_stable_in_it_is_sent_unmarked() {
        let messages = vec![LlmMessage::new("user", "hi")];
        let body = build_openai_body(&messages, &target(Protocol::OpenAi), true);

        assert!(body["messages"][0]["content"].is_string());
    }

    #[test]
    fn an_endpoint_complaining_about_cache_control_makes_us_stop_sending_it() {
        assert!(refused_the_marker(&JumabekError::LlmUnavailable(
            "400 — unknown field cache_control".to_string()
        )));
        assert!(!refused_the_marker(&JumabekError::LlmUnavailable(
            "401 — bad api key".to_string()
        )));
        assert!(!refused_the_marker(&JumabekError::LlmTimeout(
            "took too long".to_string()
        )));
    }

    #[test]
    fn anthropic_structured_output_forces_the_agent_response_tool() {
        let mut t = target(Protocol::Anthropic);
        t.structured_output = true;
        let body = build_anthropic_body(&[], &t, false);

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
                ActionType::SpawnAgent { task, reason, .. } => {
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
