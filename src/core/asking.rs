use std::collections::HashMap;

use tokio::sync::{Mutex, oneshot};

/// A permission a detached agent needs and cannot grant itself. It waits on the
/// answer, so every one of these must end in a verdict — an allow, a refusal, or
/// the clock running out. None of them may simply be dropped.
/// What is being asked. A permission needs a yes or a no; a question needs an
/// answer in words, and there is no sensible default for either.
#[derive(Debug, Clone, PartialEq)]
pub enum Ask {
    Permission {
        action: String,
        description: String,
        risk_level: String,
    },
    Question {
        message: String,
        options: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Reply {
    Allowed(bool),
    Said(String),
}

#[derive(Debug, Clone)]
pub struct Wanted {
    pub id: String,
    pub asked_by: String,
    pub role: Option<String>,
    pub ask: Ask,
}

impl Wanted {
    pub fn wants_words(&self) -> bool {
        matches!(self.ask, Ask::Question { .. })
    }

    pub fn line(&self) -> String {
        let who = match &self.role {
            Some(role) => format!("{} the {}", short(&self.asked_by), role),
            None => short(&self.asked_by),
        };

        match &self.ask {
            Ask::Permission {
                action,
                description,
                risk_level,
            } => {
                let why = if description.trim().is_empty() {
                    String::new()
                } else {
                    format!(" — {}", description.trim())
                };

                format!(
                    "{} wants to {} ({} risk){}",
                    who,
                    action.trim(),
                    risk_level.trim(),
                    why
                )
            }

            Ask::Question { message, options } => {
                let choices = if options.is_empty() {
                    String::new()
                } else {
                    format!(" [{}]", options.join(" / "))
                };

                format!("{} asks: {}{}", who, message.trim(), choices)
            }
        }
    }

    pub fn how_to_answer(&self) -> String {
        match self.ask {
            Ask::Permission { .. } => {
                format!("/allow {} · /deny {}", self.id, self.id)
            }
            Ask::Question { .. } => format!("/answer {} <your answer>", self.id),
        }
    }
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

/// Only an explicit yes is permission. A refusal, an answer in the wrong shape,
/// and nobody answering at all are each a no — silence must never be consent.
pub fn granted(reply: Option<&Reply>) -> bool {
    matches!(reply, Some(Reply::Allowed(true)))
}

#[derive(Default)]
pub struct Waiting {
    open: Mutex<HashMap<String, (Wanted, oneshot::Sender<Reply>)>>,
}

impl Waiting {
    pub fn new() -> Waiting {
        Waiting::default()
    }

    /// Register a request and hand back the end to wait on.
    pub async fn open(&self, wanted: Wanted) -> oneshot::Receiver<Reply> {
        let (tx, rx) = oneshot::channel();
        self.open
            .lock()
            .await
            .insert(wanted.id.clone(), (wanted, tx));
        rx
    }

    /// Answer one. False means there was no such request — already answered,
    /// already timed out, or an id that never existed.
    pub async fn answer(&self, id: &str, reply: Reply) -> Option<Wanted> {
        let waiting = self.open.lock().await.get(id).map(|(w, _)| w.clone())?;

        // A question needs words and a permission needs a verdict. Answering one
        // in the shape of the other would leave the asker stuck, so refuse it and
        // leave the request open for a proper answer.
        let fits = matches!(
            (&waiting.ask, &reply),
            (Ask::Permission { .. }, Reply::Allowed(_)) | (Ask::Question { .. }, Reply::Said(_))
        );

        if !fits {
            return None;
        }

        let (wanted, tx) = self.open.lock().await.remove(id)?;
        let _ = tx.send(reply);
        Some(wanted)
    }

    /// Give up on one without answering it, so a request whose asker has gone
    /// does not sit in the list forever.
    pub async fn close(&self, id: &str) {
        self.open.lock().await.remove(id);
    }

    pub async fn outstanding(&self) -> Vec<Wanted> {
        let mut all: Vec<Wanted> = self
            .open
            .lock()
            .await
            .values()
            .map(|(wanted, _)| wanted.clone())
            .collect();
        all.sort_by(|a, b| a.id.cmp(&b.id));
        all
    }
}

pub fn as_text(open: &[Wanted]) -> String {
    if open.is_empty() {
        return "nothing is waiting on you".to_string();
    }

    open.iter()
        .map(|w| format!("#{} {}\n    {}", w.id, w.line(), w.how_to_answer()))
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wanted(id: &str) -> Wanted {
        Wanted {
            id: id.to_string(),
            asked_by: "abcdef1234".to_string(),
            role: Some("researcher".to_string()),
            ask: Ask::Permission {
                action: "delete the old build".to_string(),
                description: "340 files under target/".to_string(),
                risk_level: "medium".to_string(),
            },
        }
    }

    fn question(id: &str, message: &str) -> Wanted {
        Wanted {
            id: id.to_string(),
            asked_by: "abcdef1234".to_string(),
            role: None,
            ask: Ask::Question {
                message: message.to_string(),
                options: Vec::new(),
            },
        }
    }

    #[tokio::test]
    async fn an_answer_reaches_whoever_is_waiting() {
        let waiting = Waiting::new();
        let rx = waiting.open(wanted("1")).await;

        assert!(waiting.answer("1", Reply::Allowed(true)).await.is_some());
        assert_eq!(rx.await.unwrap(), Reply::Allowed(true));
    }

    #[tokio::test]
    async fn a_refusal_reaches_them_just_as_surely() {
        let waiting = Waiting::new();
        let rx = waiting.open(wanted("1")).await;

        waiting.answer("1", Reply::Allowed(false)).await;
        assert_eq!(rx.await.unwrap(), Reply::Allowed(false));
    }

    #[tokio::test]
    async fn answering_the_same_request_twice_only_works_once() {
        let waiting = Waiting::new();
        let _rx = waiting.open(wanted("1")).await;

        assert!(waiting.answer("1", Reply::Allowed(true)).await.is_some());
        assert!(
            waiting.answer("1", Reply::Allowed(false)).await.is_none(),
            "a settled request was answered again"
        );
    }

    #[tokio::test]
    async fn an_id_nobody_asked_about_is_not_invented() {
        let waiting = Waiting::new();
        assert!(waiting.answer("nope", Reply::Allowed(true)).await.is_none());
    }

    #[tokio::test]
    async fn giving_up_on_a_request_takes_it_out_of_the_list() {
        let waiting = Waiting::new();
        let _rx = waiting.open(wanted("1")).await;
        assert_eq!(waiting.outstanding().await.len(), 1);

        waiting.close("1").await;
        assert!(waiting.outstanding().await.is_empty());
        assert!(waiting.answer("1", Reply::Allowed(true)).await.is_none());
    }

    #[tokio::test]
    async fn several_requests_wait_side_by_side_and_are_answered_apart() {
        let waiting = Waiting::new();
        let one = waiting.open(wanted("1")).await;
        let two = waiting.open(wanted("2")).await;

        waiting.answer("2", Reply::Allowed(true)).await;
        assert_eq!(two.await.unwrap(), Reply::Allowed(true));
        assert_eq!(
            waiting.outstanding().await.len(),
            1,
            "answering one settled the other as well"
        );

        waiting.answer("1", Reply::Allowed(false)).await;
        assert_eq!(one.await.unwrap(), Reply::Allowed(false));
    }

    #[tokio::test]
    async fn a_waiting_agent_whose_answer_never_comes_learns_it_was_dropped() {
        let waiting = Waiting::new();
        let rx = waiting.open(wanted("1")).await;

        waiting.close("1").await;
        assert!(
            rx.await.is_err(),
            "the asker was left waiting on a channel nobody holds"
        );
    }

    #[test]
    fn a_request_reads_as_something_a_person_can_decide_on() {
        let said = wanted("1").line();

        assert!(said.contains("delete the old build"), "{said}");
        assert!(said.contains("medium risk"), "{said}");
        assert!(said.contains("340 files"), "{said}");
        assert!(said.contains("researcher"), "{said}");
    }

    #[test]
    fn a_request_with_no_explanation_still_reads() {
        let mut w = wanted("1");
        w.role = None;
        w.ask = Ask::Permission {
            action: "delete the old build".to_string(),
            description: String::new(),
            risk_level: "medium".to_string(),
        };

        let said = w.line();
        assert!(said.contains("delete the old build"), "{said}");
        assert!(
            !said.contains("—"),
            "an empty reason left a dangling dash: {said}"
        );
    }

    #[tokio::test]
    async fn a_question_is_answered_in_words_and_the_words_arrive() {
        let waiting = Waiting::new();
        let rx = waiting.open(question("1", "which folder?")).await;

        assert!(
            waiting
                .answer("1", Reply::Said("the one in Documents".to_string()))
                .await
                .is_some()
        );
        assert_eq!(
            rx.await.unwrap(),
            Reply::Said("the one in Documents".to_string())
        );
    }

    #[tokio::test]
    async fn a_question_cannot_be_settled_with_a_yes_or_a_no() {
        let waiting = Waiting::new();
        let _rx = waiting.open(question("1", "which folder?")).await;

        assert!(
            waiting.answer("1", Reply::Allowed(true)).await.is_none(),
            "a question was closed with a verdict, leaving the asker with nothing to use"
        );
        assert_eq!(
            waiting.outstanding().await.len(),
            1,
            "the request was consumed by an answer that did not fit it"
        );
    }

    #[tokio::test]
    async fn a_permission_cannot_be_settled_with_prose() {
        let waiting = Waiting::new();
        let _rx = waiting.open(wanted("1")).await;

        assert!(
            waiting
                .answer("1", Reply::Said("go ahead I guess".to_string()))
                .await
                .is_none(),
            "words were read as consent"
        );
        assert_eq!(waiting.outstanding().await.len(), 1);
    }

    #[test]
    fn each_kind_says_how_to_answer_it() {
        assert!(wanted("1").how_to_answer().contains("/allow 1"));
        assert!(question("2", "?").how_to_answer().contains("/answer 2"));
    }

    #[test]
    fn a_question_with_choices_shows_them() {
        let mut w = question("1", "which one?");
        w.ask = Ask::Question {
            message: "which one?".to_string(),
            options: vec!["Documents".to_string(), "Downloads".to_string()],
        };

        let said = w.line();
        assert!(said.contains("Documents / Downloads"), "{said}");
    }

    #[test]
    fn an_empty_list_says_so() {
        assert_eq!(as_text(&[]), "nothing is waiting on you");
    }

    #[test]
    fn only_an_explicit_yes_counts_as_permission() {
        assert!(granted(Some(&Reply::Allowed(true))));
        assert!(!granted(Some(&Reply::Allowed(false))));
        assert!(!granted(None), "silence was taken for consent");
        assert!(
            !granted(Some(&Reply::Said("sure, go ahead".to_string()))),
            "prose was taken for permission"
        );
    }
}
