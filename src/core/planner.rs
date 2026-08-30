use crate::core::task::ActionType;

pub const MAX_PARALLEL: usize = 4;

#[derive(Debug, Clone, PartialEq)]
pub enum Stage {
    Single(ActionType),
    Parallel(Vec<ActionType>),
}

impl Stage {
    #[cfg(test)]
    pub fn len(&self) -> usize {
        match self {
            Stage::Single(_) => 1,
            Stage::Parallel(actions) => actions.len(),
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct ExecutionPlan {
    pub stages: Vec<Stage>,
}

impl ExecutionPlan {
    #[cfg(test)]
    pub fn action_count(&self) -> usize {
        self.stages.iter().map(|s| s.len()).sum()
    }

    pub fn parallel_groups(&self) -> usize {
        self.stages
            .iter()
            .filter(|s| matches!(s, Stage::Parallel(_)))
            .count()
    }

    pub fn describe(&self) -> String {
        self.stages
            .iter()
            .map(|stage| match stage {
                Stage::Single(action) => short_name(action).to_string(),
                Stage::Parallel(actions) => format!(
                    "[{}]",
                    actions
                        .iter()
                        .map(|a| short_name(a).to_string())
                        .collect::<Vec<_>>()
                        .join(" + ")
                ),
            })
            .collect::<Vec<_>>()
            .join(" -> ")
    }
}

fn short_name(action: &ActionType) -> &str {
    match action {
        ActionType::ExecuteModule { module, .. } => module,
        ActionType::RespondToUser => "respond",
        ActionType::PermissionRequest { .. } => "permission",
        ActionType::PromptToUser { .. } => "prompt",
        ActionType::RequestData { .. } => "memory",
        ActionType::SpawnAgent { .. } => "subagent",
        ActionType::RequestInboxKey { .. } => "inbox key",
        ActionType::Remember { .. } => "remember",
        ActionType::Forget { .. } => "forget",
        ActionType::ScheduleJob { .. } => "job",
        ActionType::ManageJobs { .. } => "jobs",
        ActionType::GenerateChunk { .. } => "chunk",
        ActionType::Switch { .. } => "switch",
        ActionType::PostToBoard { .. } => "board",
        ActionType::AskAgent { .. } => "ask agent",
        ActionType::RequestGrant { .. } => "rights",
    }
}

fn parallel_module(action: &ActionType) -> Option<&str> {
    match action {
        ActionType::ExecuteModule {
            module, parallel, ..
        } if *parallel => Some(module),
        _ => None,
    }
}

pub fn plan(actions: &[ActionType]) -> ExecutionPlan {
    let mut stages: Vec<Stage> = Vec::new();
    let mut group: Vec<ActionType> = Vec::new();
    let mut group_modules: Vec<String> = Vec::new();

    let flush =
        |stages: &mut Vec<Stage>, group: &mut Vec<ActionType>, modules: &mut Vec<String>| {
            match group.len() {
                0 => {}
                1 => stages.push(Stage::Single(group.remove(0))),
                _ => stages.push(Stage::Parallel(std::mem::take(group))),
            }
            group.clear();
            modules.clear();
        };

    for action in actions {
        match parallel_module(action) {
            Some(module) => {
                let same_skill_busy = group_modules.iter().any(|m| m == module);
                if same_skill_busy || group.len() >= MAX_PARALLEL {
                    flush(&mut stages, &mut group, &mut group_modules);
                }
                group_modules.push(module.to_string());
                group.push(action.clone());
            }
            None => {
                flush(&mut stages, &mut group, &mut group_modules);
                stages.push(Stage::Single(action.clone()));
            }
        }
    }

    flush(&mut stages, &mut group, &mut group_modules);

    ExecutionPlan { stages }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn exec(module: &str, parallel: bool) -> ActionType {
        ActionType::ExecuteModule {
            module: module.to_string(),
            method: "run".to_string(),
            args: "x".to_string(),
            parallel,
        }
    }

    fn prompt() -> ActionType {
        ActionType::PromptToUser {
            message: "?".to_string(),
            options: Vec::new(),
        }
    }

    #[test]
    fn sequential_actions_stay_sequential() {
        let plan = plan(&[exec("a", false), exec("b", false)]);
        assert_eq!(plan.stages.len(), 2);
        assert_eq!(plan.parallel_groups(), 0);
        assert_eq!(plan.describe(), "a -> b");
    }

    #[test]
    fn parallel_actions_on_different_skills_group_up() {
        let plan = plan(&[exec("a", true), exec("b", true), exec("c", true)]);
        assert_eq!(plan.stages.len(), 1);
        assert_eq!(plan.describe(), "[a + b + c]");
    }

    #[test]
    fn the_same_skill_twice_is_never_parallel() {
        let plan = plan(&[exec("shell", true), exec("shell", true)]);
        assert_eq!(plan.parallel_groups(), 0, "one pipe cannot serve two calls");
        assert_eq!(plan.describe(), "shell -> shell");
    }

    #[test]
    fn interactive_actions_break_a_group() {
        let plan = plan(&[exec("a", true), prompt(), exec("b", true)]);
        assert_eq!(plan.describe(), "a -> prompt -> b");
    }

    #[test]
    fn a_sequential_action_between_parallel_ones_splits_them() {
        let plan = plan(&[exec("a", true), exec("b", false), exec("c", true)]);
        assert_eq!(plan.describe(), "a -> b -> c");
    }

    #[test]
    fn groups_are_capped() {
        let actions: Vec<ActionType> = (0..MAX_PARALLEL + 2)
            .map(|i| exec(&format!("skill{}", i), true))
            .collect();

        let plan = plan(&actions);
        assert_eq!(plan.action_count(), actions.len());
        for stage in &plan.stages {
            assert!(stage.len() <= MAX_PARALLEL, "group over the cap");
        }
    }

    #[test]
    fn nothing_is_lost_or_reordered() {
        let actions = vec![
            exec("a", true),
            exec("b", true),
            prompt(),
            exec("c", false),
            exec("d", true),
        ];
        let plan = plan(&actions);

        let flat: Vec<ActionType> = plan
            .stages
            .iter()
            .flat_map(|stage| match stage {
                Stage::Single(a) => vec![a.clone()],
                Stage::Parallel(list) => list.clone(),
            })
            .collect();

        assert_eq!(flat, actions);
    }

    #[test]
    fn an_empty_response_makes_an_empty_plan() {
        let plan = plan(&[]);
        assert!(plan.stages.is_empty());
        assert_eq!(plan.action_count(), 0);
    }
}
