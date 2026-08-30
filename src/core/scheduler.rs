use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use tokio::sync::Mutex;

use crate::core::agent::Agent;
use crate::core::jobs::{self, Job, Schedule};
use crate::core::task::Choice;
use crate::error::{JumabekError, JumabekResult};
use crate::interfaces::UserInterface;

const TICK: std::time::Duration = std::time::Duration::from_secs(1);

pub trait Notifier: Send + Sync {
    fn notify(&self, text: String);
}

pub struct PlainNotifier;

impl Notifier for PlainNotifier {
    fn notify(&self, text: String) {
        println!("{}", text);
    }
}

pub struct SharedNotifier {
    inner: std::sync::RwLock<Arc<dyn Notifier>>,
}

impl SharedNotifier {
    pub fn new(inner: Arc<dyn Notifier>) -> Self {
        SharedNotifier {
            inner: std::sync::RwLock::new(inner),
        }
    }

    pub fn replace(&self, next: Arc<dyn Notifier>) {
        if let Ok(mut current) = self.inner.write() {
            *current = next;
        }
    }
}

impl Notifier for SharedNotifier {
    fn notify(&self, text: String) {
        let current = match self.inner.read() {
            Ok(current) => Arc::clone(&current),
            Err(_) => return,
        };
        current.notify(text);
    }
}

struct JobUi {
    notifier: Arc<dyn Notifier>,
    name: String,
    id: i64,
}

pub fn detached_ui() -> impl UserInterface {
    JobUi {
        notifier: Arc::new(SilentNotifier),
        name: "inbox".to_string(),
        id: 0,
    }
}

struct SilentNotifier;

impl Notifier for SilentNotifier {
    fn notify(&self, _text: String) {}
}

impl JobUi {
    fn line(&self, marker: &str, text: &str) {
        self.notifier.notify(format!(
            "  {} job {} · {} · {}",
            marker, self.id, self.name, text
        ));
    }
}

#[async_trait::async_trait]
impl UserInterface for JobUi {
    async fn read_request(&mut self) -> JumabekResult<Option<String>> {
        Ok(None)
    }

    async fn show_response(&mut self, text: &str) -> JumabekResult<()> {
        for line in text.lines() {
            self.line("|", line);
        }
        Ok(())
    }

    async fn show_status(&mut self, _text: &str) -> JumabekResult<()> {
        Ok(())
    }

    async fn show_error(&mut self, text: &str) -> JumabekResult<()> {
        self.line("x", text);
        Ok(())
    }

    async fn ask_permission(&mut self, action: &str, _: &str, _: &str) -> JumabekResult<bool> {
        self.line(
            "x",
            &format!(
                "wanted permission for '{}' — denied, nobody is here",
                action
            ),
        );
        Ok(false)
    }

    async fn prompt_choice(&mut self, message: &str, options: &[Choice]) -> JumabekResult<String> {
        self.line(
            "x",
            &format!("wanted to ask '{}' — nobody is here", message),
        );
        Err(JumabekError::InternalError(format!(
            "a background job tried to ask a question with {} option(s)",
            options.len()
        )))
    }
}

pub struct Scheduler {
    agent: Arc<Agent>,
    notifier: Arc<dyn Notifier>,
    seen: Mutex<HashMap<i64, Vec<String>>>,
}

impl Scheduler {
    pub fn new(agent: Arc<Agent>, notifier: Arc<dyn Notifier>) -> Self {
        Scheduler {
            agent,
            notifier,
            seen: Mutex::new(HashMap::new()),
        }
    }

    pub fn spawn(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(TICK).await;
                if let Err(e) = self.tick().await {
                    self.notifier.notify(format!("  x scheduler: {}", e));
                }
            }
        });
    }

    async fn tick(&self) -> JumabekResult<()> {
        for job in self.agent.jobs().due(Utc::now()).await? {
            self.run_one(job).await?;
        }
        Ok(())
    }

    async fn run_one(&self, job: Job) -> JumabekResult<()> {
        let trigger = match &job.schedule {
            Schedule::Watch { path, .. } => {
                let now = jobs::snapshot(path);
                let mut seen = self.seen.lock().await;

                let Some(before) = seen.insert(job.id, now.clone()) else {
                    self.agent.jobs().finish_run(job.id, "first look").await?;
                    return Ok(());
                };

                let moved = jobs::changes(&before, &now);
                if moved.is_empty() {
                    self.agent.jobs().finish_run(job.id, "no change").await?;
                    return Ok(());
                }

                Some(moved.join(", "))
            }
            _ => None,
        };

        let mut task = match &trigger {
            Some(changed) => format!(
                "{}\n\nWhat changed since the last look: {}",
                job.task, changed
            ),
            None => job.task.clone(),
        };

        if let Some(last) = &job.last_result {
            let last = last.trim();
            if !last.is_empty() {
                task.push_str(&format!(
                    "\n\nYou have run this before. Last time, at {}, you reported:\n{}\n\n\
                     Do not repeat work you already finished, and do not send the same message \
                     twice.",
                    job.last_run.as_deref().unwrap_or("an earlier time"),
                    last
                ));
            }
        }

        self.notifier
            .notify(format!("  · job {} · {} · running", job.id, job.name));

        let mut ui = JobUi {
            notifier: Arc::clone(&self.notifier),
            name: job.name.clone(),
            id: job.id,
        };

        let outcome = self
            .agent
            .run_job(&mut ui, task, job.grant.clone(), job.id)
            .await
            .unwrap_or_else(|e| format!("failed: {}", e));

        self.agent.jobs().finish_run(job.id, &outcome).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    struct Collector {
        lines: StdMutex<Vec<String>>,
    }

    impl Notifier for Collector {
        fn notify(&self, text: String) {
            self.lines.lock().unwrap().push(text);
        }
    }

    fn ui(collector: Arc<Collector>) -> JobUi {
        JobUi {
            notifier: collector,
            name: "watcher".to_string(),
            id: 7,
        }
    }

    #[tokio::test]
    async fn a_job_cannot_be_granted_permission_by_itself() {
        let collector = Arc::new(Collector {
            lines: StdMutex::new(Vec::new()),
        });
        let mut ui = ui(Arc::clone(&collector));

        let allowed = ui
            .ask_permission("delete everything", "", "high")
            .await
            .unwrap();

        assert!(!allowed, "a background job granted itself permission");
        let lines = collector.lines.lock().unwrap();
        assert!(
            lines[0].contains("nobody is here"),
            "the refusal was not reported: {:?}",
            lines
        );
    }

    #[tokio::test]
    async fn a_job_reading_input_gets_nothing() {
        let collector = Arc::new(Collector {
            lines: StdMutex::new(Vec::new()),
        });
        assert!(ui(collector).read_request().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn a_job_answer_is_labelled_with_its_number() {
        let collector = Arc::new(Collector {
            lines: StdMutex::new(Vec::new()),
        });
        let mut ui = ui(Arc::clone(&collector));

        ui.show_response("two files arrived\nboth are PDFs")
            .await
            .unwrap();

        let lines = collector.lines.lock().unwrap();
        assert_eq!(lines.len(), 2, "multi-line output was not split");
        assert!(lines.iter().all(|l| l.contains("job 7")));
        assert!(lines[1].contains("both are PDFs"));
    }
}
