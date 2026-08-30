use std::collections::HashSet;

use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

use crate::{
    SkillError, SkillModule,
    protocol::{ExecuteParams, SkillRequest, SkillResponse, SkillResponsePayload},
};

fn declared_methods<S: SkillModule>(skill: &S) -> HashSet<String> {
    skill
        .available_methods()
        .into_iter()
        .map(|info| info.method)
        .collect()
}

fn unknown_method(method: &str, declared: &HashSet<String>) -> SkillError {
    let mut names: Vec<&str> = declared.iter().map(String::as_str).collect();
    names.sort_unstable();

    SkillError::NotFound(if names.is_empty() {
        format!("no method '{}': this skill declares none", method)
    } else {
        format!("no method '{}'. Available: {}", method, names.join(", "))
    })
}

async fn dispatch<S: SkillModule>(
    skill: &S,
    declared: &HashSet<String>,
    request: SkillRequest,
) -> SkillResponse {
    let payload = match request.method.as_str() {
        "health_check" => SkillResponsePayload::Health(skill.health_check()),

        "available_methods" => SkillResponsePayload::Methods(skill.available_methods()),

        "get_metadata" => SkillResponsePayload::Metadata(skill.get_metadata().clone()),

        "execute" => match request.params {
            Some(params) => match serde_json::from_str::<ExecuteParams>(&params) {
                Ok(parameters) => {
                    if declared.contains(&parameters.method) {
                        let call = crate::CALLER.scope(
                            parameters.caller.clone(),
                            skill.execute(&parameters.method, &parameters.args),
                        );
                        match call.await {
                            Ok(output) => SkillResponsePayload::Output(output),
                            Err(error) => SkillResponsePayload::Error(error),
                        }
                    } else {
                        SkillResponsePayload::Error(unknown_method(&parameters.method, declared))
                    }
                }

                Err(error) => {
                    SkillResponsePayload::Error(SkillError::InvalidArgs(error.to_string()))
                }
            },
            None => SkillResponsePayload::Error(SkillError::InvalidArgs(
                "Not provided any parameters".to_string(),
            )),
        },

        _ => SkillResponsePayload::Error(SkillError::NotFound("Method not found".to_string())),
    };

    SkillResponse {
        id: request.id,
        payload,
    }
}

pub async fn run_skill<S: SkillModule>(skill: S) -> std::io::Result<()> {
    let declared = declared_methods(&skill);

    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();

    while let Some(line) = lines.next_line().await? {
        let response = match serde_json::from_str::<SkillRequest>(&line) {
            Ok(request) => dispatch(&skill, &declared, request).await,
            Err(error) => SkillResponse {
                id: 0,
                payload: SkillResponsePayload::Error(SkillError::InvalidArgs(error.to_string())),
            },
        };

        let json = serde_json::to_string(&response)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        out.write_all(json.as_bytes()).await?;
        out.write_all(b"\n").await?;
        out.flush().await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MethodInfo, ModuleMetadata, SkillOutput};
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Counted {
        metadata: ModuleMetadata,
        entered: AtomicUsize,
    }

    impl Counted {
        fn new() -> Self {
            Counted {
                metadata: ModuleMetadata {
                    name: "counted".to_string(),
                    version: "1.0.0".to_string(),
                    description: "counts how often execute was entered".to_string(),
                },
                entered: AtomicUsize::new(0),
            }
        }

        fn entered(&self) -> usize {
            self.entered.load(Ordering::SeqCst)
        }
    }

    #[async_trait::async_trait]
    impl SkillModule for Counted {
        fn get_metadata(&self) -> &ModuleMetadata {
            &self.metadata
        }

        fn health_check(&self) -> bool {
            true
        }

        fn available_methods(&self) -> Vec<MethodInfo> {
            vec![MethodInfo {
                method: "count".to_string(),
                description: "counts the words in a piece of text".to_string(),
                args_description: "the text to count".to_string(),
            }]
        }

        async fn execute(&self, _method: &str, args: &str) -> Result<SkillOutput, SkillError> {
            self.entered.fetch_add(1, Ordering::SeqCst);
            Ok(SkillOutput::Text(
                args.split_whitespace().count().to_string(),
            ))
        }
    }

    fn execute_request(method: &str, args: &str) -> SkillRequest {
        SkillRequest {
            id: 1,
            method: "execute".to_string(),
            params: Some(serde_json::json!({ "method": method, "args": args }).to_string()),
        }
    }

    #[tokio::test]
    async fn a_method_the_skill_never_declared_is_refused_before_execute_runs() {
        let skill = Counted::new();
        let declared = declared_methods(&skill);

        let response = dispatch(&skill, &declared, execute_request("wander_off", "")).await;

        match response.payload {
            SkillResponsePayload::Error(SkillError::NotFound(detail)) => {
                assert!(detail.contains("wander_off"), "{detail}");
                assert!(
                    detail.contains("count"),
                    "the reply hides what is available: {detail}"
                );
            }
            other => panic!("an undeclared method reached the skill: {other:?}"),
        }

        assert_eq!(
            skill.entered(),
            0,
            "execute ran for a method the skill never declared"
        );
    }

    #[tokio::test]
    async fn execute_params_survive_a_request_that_names_no_caller() {
        let old: ExecuteParams =
            serde_json::from_str(r#"{"method":"count","args":"one two"}"#).expect("old shape");

        assert_eq!(old.method, "count");
        assert_eq!(old.caller, None, "a caller appeared where none was sent");
    }

    #[tokio::test]
    async fn a_declared_method_still_reaches_execute() {
        let skill = Counted::new();
        let declared = declared_methods(&skill);

        let response = dispatch(&skill, &declared, execute_request("count", "one two three")).await;

        match response.payload {
            SkillResponsePayload::Output(SkillOutput::Text(text)) => assert_eq!(text, "3"),
            other => panic!("a declared method was blocked: {other:?}"),
        }

        assert_eq!(skill.entered(), 1);
    }

    #[tokio::test]
    async fn the_refusal_is_the_same_shape_the_validator_probes_with() {
        let skill = Counted::new();
        let declared = declared_methods(&skill);

        let probe = dispatch(
            &skill,
            &declared,
            execute_request("__jumabek_probe_no_such_method__", ""),
        )
        .await;
        let other = dispatch(&skill, &declared, execute_request("also_missing", "")).await;

        let kind = |payload: &SkillResponsePayload| {
            matches!(
                payload,
                SkillResponsePayload::Error(SkillError::NotFound(_))
            )
        };
        assert!(
            kind(&probe.payload) && kind(&other.payload),
            "two undeclared methods answered differently, so the baseline proves nothing"
        );
    }

    #[tokio::test]
    async fn asking_what_the_skill_offers_is_untouched() {
        let skill = Counted::new();
        let declared = declared_methods(&skill);

        let response = dispatch(
            &skill,
            &declared,
            SkillRequest {
                id: 7,
                method: "available_methods".to_string(),
                params: None,
            },
        )
        .await;

        assert_eq!(response.id, 7);
        match response.payload {
            SkillResponsePayload::Methods(methods) => {
                assert_eq!(methods.len(), 1);
                assert_eq!(methods[0].method, "count");
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(skill.entered(), 0);
    }
}
