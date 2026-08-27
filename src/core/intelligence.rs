use serde::{Deserialize, Serialize};

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    Low,
    #[default]
    Medium,
    High,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Low, Level::Medium, Level::High];

    pub fn parse(raw: &str) -> Option<Level> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "low" | "fast" | "cheap" => Some(Level::Low),
            "medium" | "normal" | "default" => Some(Level::Medium),
            "high" | "smart" | "max" => Some(Level::High),
            _ => None,
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
        }
    }
}

impl std::fmt::Display for Level {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Standing {
    pub level: Level,
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changed_from: Option<Level>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
    #[serde(skip)]
    pub reason: Option<Reason>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reason {
    UnreadableAnswer,
    BuildAttempts,
    WritingASkill,
    Circling,
    NobodyWatching,
    TaskFinished,
    ModelAsked,
}

impl Reason {
    pub fn explain(self) -> &'static str {
        match self {
            Reason::UnreadableAnswer => {
                "the previous answer could not be read as an agent response twice running"
            }
            Reason::BuildAttempts => "the skill failed to build more than once",
            Reason::WritingASkill => "writing a skill always runs at the highest level",
            Reason::Circling => {
                "the same step repeated without progress, or the task is nearly out of iterations"
            }
            Reason::NobodyWatching => "nobody is at the keyboard for this one",
            Reason::TaskFinished => "back to the default level for a new task",
            Reason::ModelAsked => "you asked for it",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Reason::UnreadableAnswer => "unreadable_answer",
            Reason::BuildAttempts => "build_attempts",
            Reason::WritingASkill => "writing_a_skill",
            Reason::Circling => "circling",
            Reason::NobodyWatching => "nobody_watching",
            Reason::TaskFinished => "task_finished",
            Reason::ModelAsked => "model_asked",
        }
    }

    pub fn escalation_from(self, current: Level) -> Level {
        match self {
            Reason::WritingASkill => Level::High,
            _ => match current {
                Level::Low => Level::Medium,
                Level::Medium | Level::High => Level::High,
            },
        }
    }

    pub fn refunds_the_iteration(self) -> bool {
        matches!(self, Reason::UnreadableAnswer | Reason::Circling)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_compare_in_the_order_they_cost() {
        assert!(Level::Low < Level::Medium);
        assert!(Level::Medium < Level::High);
    }

    #[test]
    fn the_default_is_the_middle_one() {
        assert_eq!(Level::default(), Level::Medium);
    }

    #[test]
    fn every_name_a_model_might_write_lands_somewhere() {
        for (raw, expected) in [
            ("low", Level::Low),
            ("CHEAP", Level::Low),
            ("medium", Level::Medium),
            (" High ", Level::High),
            ("max", Level::High),
        ] {
            assert_eq!(Level::parse(raw), Some(expected), "on {raw:?}");
        }
    }

    #[test]
    fn an_unknown_level_is_refused_rather_than_guessed() {
        for raw in ["genius", "opus", "3", ""] {
            assert_eq!(Level::parse(raw), None, "{raw} was quietly accepted");
        }
    }

    #[test]
    fn only_a_failure_that_was_not_the_tasks_fault_refunds_the_iteration() {
        assert!(Reason::UnreadableAnswer.refunds_the_iteration());
        assert!(Reason::Circling.refunds_the_iteration());
        assert!(!Reason::WritingASkill.refunds_the_iteration());
        assert!(!Reason::ModelAsked.refunds_the_iteration());
    }

    #[test]
    fn writing_a_skill_goes_straight_to_the_top_from_any_level() {
        for start in Level::ALL {
            assert_eq!(
                Reason::WritingASkill.escalation_from(start),
                Level::High,
                "a skill was written at less than the highest level, starting from {start}"
            );
        }
    }

    #[test]
    fn an_ordinary_failure_climbs_one_step_at_a_time() {
        assert_eq!(
            Reason::UnreadableAnswer.escalation_from(Level::Low),
            Level::Medium,
            "one bad turn should try the next level, not the dearest one"
        );
        assert_eq!(
            Reason::UnreadableAnswer.escalation_from(Level::Medium),
            Level::High
        );
        assert_eq!(
            Reason::Circling.escalation_from(Level::High),
            Level::High,
            "there is nothing above the top"
        );
    }

    #[test]
    fn every_reason_has_an_id_that_can_be_counted() {
        let ids: Vec<&str> = [
            Reason::UnreadableAnswer,
            Reason::BuildAttempts,
            Reason::WritingASkill,
            Reason::Circling,
            Reason::NobodyWatching,
            Reason::TaskFinished,
            Reason::ModelAsked,
        ]
        .iter()
        .map(|r| r.id())
        .collect();

        let mut unique = ids.clone();
        unique.sort_unstable();
        unique.dedup();
        assert_eq!(unique.len(), ids.len(), "two reasons share an id: {ids:?}");

        for id in ids {
            assert!(
                id.chars().all(|c| c.is_ascii_lowercase() || c == '_'),
                "{id}"
            );
        }
    }

    #[test]
    fn every_reason_says_something_a_person_could_read() {
        for reason in [
            Reason::UnreadableAnswer,
            Reason::BuildAttempts,
            Reason::WritingASkill,
            Reason::Circling,
            Reason::NobodyWatching,
            Reason::TaskFinished,
            Reason::ModelAsked,
        ] {
            assert!(reason.explain().len() > 10, "{reason:?}");
        }
    }
}
