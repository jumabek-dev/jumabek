use crate::core::task::{LlmMessage, TaskObject};
use crate::error::{JumabekError, JumabekResult};
use crate::memory::StoredMessage;
use crate::token_counter;

const BUDGET_PERCENT: usize = 85;

#[derive(Clone)]
pub struct ContextBuilder {
    system_prompt: String,
    budget: usize,
}

#[derive(Debug, Clone)]
pub struct BuiltContext {
    pub messages: Vec<LlmMessage>,
    pub total_tokens: usize,
    pub trimmed_messages: usize,
}

impl ContextBuilder {
    pub fn new(system_prompt: impl Into<String>, context_token_limit: u32) -> Self {
        ContextBuilder {
            system_prompt: system_prompt.into(),
            budget: context_token_limit as usize * BUDGET_PERCENT / 100,
        }
    }

    pub fn rescaled(&self, context_token_limit: u32) -> Self {
        ContextBuilder {
            system_prompt: self.system_prompt.clone(),
            budget: context_token_limit as usize * BUDGET_PERCENT / 100,
        }
    }

    #[cfg(test)]
    pub fn build(
        &self,
        history: &[StoredMessage],
        current: &TaskObject,
    ) -> JumabekResult<BuiltContext> {
        self.build_with_profile(history, current, "")
    }

    pub fn build_with_profile(
        &self,
        history: &[StoredMessage],
        current: &TaskObject,
        profile: &str,
    ) -> JumabekResult<BuiltContext> {
        let current_json = serde_json::to_string(current)
            .map_err(|e| JumabekError::ParseError(format!("cannot encode task object: {}", e)))?;

        let profile_tokens = if profile.is_empty() {
            0
        } else {
            token_counter::count_message("system", profile)
        };

        let system_tokens =
            token_counter::count_message("system", &self.system_prompt) + profile_tokens;
        let current_tokens = token_counter::count_message("user", &current_json);
        let anchors = system_tokens + current_tokens;

        if anchors > self.budget {
            return Err(JumabekError::InternalError(format!(
                "system prompt and task object alone need {} tokens, budget is {}",
                anchors, self.budget
            )));
        }

        let groups = group_by_task(history);
        let (kept, trimmed_messages) = fit_groups(groups, self.budget - anchors);

        let mut messages = Vec::with_capacity(kept.len() + 3);
        messages.push(LlmMessage {
            role: "system".to_string(),
            content: self.system_prompt.clone(),
        });

        if !profile.is_empty() {
            messages.push(LlmMessage {
                role: "system".to_string(),
                content: profile.to_string(),
            });
        }

        if trimmed_messages > 0 {
            messages.push(LlmMessage {
                role: "user".to_string(),
                content: format!(
                    "[{} earlier messages were trimmed from this context. \
                     They are still stored — use RequestData with source \"memory\" to recall them.]",
                    trimmed_messages
                ),
            });
        }

        messages.extend(kept);
        messages.push(LlmMessage {
            role: "user".to_string(),
            content: current_json,
        });

        let total_tokens = messages
            .iter()
            .map(|m| token_counter::count_message(&m.role, &m.content))
            .sum();

        Ok(BuiltContext {
            messages,
            total_tokens,
            trimmed_messages,
        })
    }
}

fn llm_role(stored: &StoredMessage) -> Option<&'static str> {
    match stored.role.as_str() {
        "user" => Some("user"),
        "assistant" => Some("assistant"),
        _ => None,
    }
}

fn group_by_task(history: &[StoredMessage]) -> Vec<Vec<LlmMessage>> {
    let mut groups: Vec<Vec<LlmMessage>> = Vec::new();
    let mut current_key: Option<String> = None;

    for stored in history {
        let Some(role) = llm_role(stored) else {
            continue;
        };

        let key = stored
            .task_id
            .clone()
            .unwrap_or_else(|| format!("__msg_{}", stored.id));

        let message = LlmMessage {
            role: role.to_string(),
            content: stored.llm_content().to_string(),
        };

        if current_key.as_deref() == Some(key.as_str())
            && let Some(last) = groups.last_mut()
        {
            last.push(message);
            continue;
        }

        groups.push(vec![message]);
        current_key = Some(key);
    }

    groups
}

fn fit_groups(groups: Vec<Vec<LlmMessage>>, budget: usize) -> (Vec<LlmMessage>, usize) {
    let mut kept_rev: Vec<Vec<LlmMessage>> = Vec::new();
    let mut used = 0usize;
    let mut trimmed = 0usize;
    let mut exhausted = false;

    for group in groups.into_iter().rev() {
        let cost: usize = group
            .iter()
            .map(|m| token_counter::count_message(&m.role, &m.content))
            .sum();

        if !exhausted && used + cost <= budget {
            used += cost;
            kept_rev.push(group);
        } else {
            exhausted = true;
            trimmed += group.len();
        }
    }

    kept_rev.reverse();
    (kept_rev.into_iter().flatten().collect(), trimmed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::task::{Constraints, InterfaceMode, SystemInfo};

    fn stored(id: i64, task: &str, role: &str, content: &str) -> StoredMessage {
        StoredMessage {
            id,
            task_id: Some(task.to_string()),
            role: role.to_string(),
            content: content.to_string(),
            raw_json: None,
        }
    }

    fn task_object() -> TaskObject {
        TaskObject {
            task_id: "t-now".to_string(),
            parent_task_id: None,
            message: "открой doc.txt".to_string(),
            system_info: SystemInfo {
                os: "Windows 11".to_string(),
                shell: "powershell".to_string(),
                current_time: "2026-07-28T00:00:00Z".to_string(),
            },
            system_response: None,
            skills: Vec::new(),
            capabilities: vec!["execute_module".to_string()],
            constraints: Constraints {
                max_iterations: 10,
                max_fix_iterations: 5,
            },
            iteration: 0,
            fix_iteration: 0,
            depth: 0,
            grant: None,
            origin: None,
            intelligence: None,
            interface_mode: InterfaceMode::Cli,
        }
    }

    #[test]
    fn the_profile_rides_along_with_the_system_prompt() {
        let built = ContextBuilder::new("you are jumabek", 10_000)
            .build_with_profile(&[], &task_object(), "олжас — alias: балык")
            .unwrap();

        assert_eq!(built.messages[0].role, "system");
        assert_eq!(built.messages[1].role, "system");
        assert!(built.messages[1].content.contains("балык"));
    }

    #[test]
    fn an_empty_profile_adds_no_message() {
        let built = ContextBuilder::new("you are jumabek", 10_000)
            .build_with_profile(&[], &task_object(), "")
            .unwrap();

        assert_eq!(
            built.messages.iter().filter(|m| m.role == "system").count(),
            1
        );
    }

    #[test]
    fn a_profile_too_big_for_the_budget_is_refused_rather_than_silently_dropped() {
        let huge = "fact ".repeat(20_000);
        let result =
            ContextBuilder::new("short", 1_000).build_with_profile(&[], &task_object(), &huge);

        assert!(
            result.is_err(),
            "an oversized profile was quietly ignored instead of reported"
        );
    }

    #[test]
    fn rescaled_changes_the_budget_but_keeps_the_prompt() {
        let huge = "fact ".repeat(20_000);
        let narrow = ContextBuilder::new("short", 1_000);
        assert!(
            narrow
                .build_with_profile(&[], &task_object(), &huge)
                .is_err()
        );

        let widened = narrow.rescaled(200_000);
        assert!(
            widened
                .build_with_profile(&[], &task_object(), &huge)
                .is_ok()
        );
        assert_eq!(widened.system_prompt, narrow.system_prompt);
    }

    #[test]
    fn keeps_everything_when_it_fits() {
        let history = vec![
            stored(1, "t-1", "user", "открой файл"),
            stored(2, "t-1", "assistant", "открываю"),
        ];
        let built = ContextBuilder::new("SYSTEM", 128_000)
            .build(&history, &task_object())
            .unwrap();

        assert_eq!(built.trimmed_messages, 0);
        assert_eq!(built.messages.len(), 4);
        assert_eq!(built.messages[0].role, "system");
        assert_eq!(built.messages.last().unwrap().role, "user");
    }

    #[test]
    fn skips_skill_and_system_rows() {
        let history = vec![
            stored(1, "t-1", "user", "собери проект"),
            stored(2, "t-1", "skill", "cargo build ... 9000 lines"),
            stored(3, "t-1", "system", "internal note"),
            stored(4, "t-1", "assistant", "готово"),
        ];
        let built = ContextBuilder::new("SYSTEM", 128_000)
            .build(&history, &task_object())
            .unwrap();

        let bodies: Vec<&str> = built.messages.iter().map(|m| m.content.as_str()).collect();
        assert!(!bodies.iter().any(|b| b.contains("9000 lines")));
        assert!(!bodies.iter().any(|b| b.contains("internal note")));
    }

    #[test]
    fn prefers_raw_json_over_content() {
        let mut row = stored(1, "t-1", "assistant", "готово");
        row.raw_json = Some(r#"{"message":"готово","actions":[]}"#.to_string());

        let built = ContextBuilder::new("SYSTEM", 128_000)
            .build(&[row], &task_object())
            .unwrap();

        assert!(built.messages[1].content.contains("\"actions\""));
    }

    #[test]
    fn trims_oldest_and_marks_it() {
        let big = "x ".repeat(400);
        let history: Vec<StoredMessage> = (1..=20)
            .map(|i| {
                stored(
                    i,
                    &format!("t-{}", i),
                    if i % 2 == 0 { "assistant" } else { "user" },
                    &big,
                )
            })
            .collect();

        let built = ContextBuilder::new("SYSTEM", 4_000)
            .build(&history, &task_object())
            .unwrap();

        assert!(built.trimmed_messages > 0, "nothing was trimmed");
        assert!(built.total_tokens <= 4_000 * BUDGET_PERCENT / 100);
        assert!(built.messages[1].content.contains("were trimmed"));
        assert!(
            built
                .messages
                .last()
                .unwrap()
                .content
                .contains("открой doc.txt")
        );
    }

    #[test]
    fn never_splits_a_task_group() {
        let big = "y ".repeat(300);
        let history = vec![
            stored(1, "t-old", "user", &big),
            stored(2, "t-old", "assistant", &big),
            stored(3, "t-new", "user", "коротко"),
            stored(4, "t-new", "assistant", "ок"),
        ];

        let built = ContextBuilder::new("SYSTEM", 2_000)
            .build(&history, &task_object())
            .unwrap();

        let kept: Vec<&str> = built.messages.iter().map(|m| m.content.as_str()).collect();
        let kept_old = kept.iter().filter(|c| c.starts_with("y ")).count();
        assert!(
            kept_old == 0 || kept_old == 2,
            "task group was split: {} of 2 kept",
            kept_old
        );
    }

    #[test]
    fn errors_when_anchors_alone_overflow() {
        let huge = "z ".repeat(5_000);
        let result = ContextBuilder::new(huge, 1_000).build(&[], &task_object());
        assert!(result.is_err());
    }
}
