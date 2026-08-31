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

/// Everything that speaks from a background task — a job, an agent working on
/// its own, the inbox — goes through here, and none of it is allowed to land in
/// the middle of an answer being written to the screen. While the gate is held
/// the lines queue up in the order they were said, and they are let out in that
/// order once the screen is free.
pub struct SharedNotifier {
    inner: std::sync::RwLock<Arc<dyn Notifier>>,
    held: std::sync::atomic::AtomicUsize,
    queued: std::sync::Mutex<Vec<String>>,
    dropped: std::sync::atomic::AtomicBool,
}

const QUEUE_LIMIT: usize = 200;

impl SharedNotifier {
    pub fn new(inner: Arc<dyn Notifier>) -> Self {
        SharedNotifier {
            inner: std::sync::RwLock::new(inner),
            held: std::sync::atomic::AtomicUsize::new(0),
            queued: std::sync::Mutex::new(Vec::new()),
            dropped: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn replace(&self, next: Arc<dyn Notifier>) {
        if let Ok(mut current) = self.inner.write() {
            *current = next;
        }
    }

    /// Nothing reaches the screen until the matching `release`. Holds nest, so a
    /// turn inside a turn does not open the gate early.
    pub fn hold(&self) {
        self.held.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn release(&self) {
        let before = self.held.fetch_sub(1, std::sync::atomic::Ordering::SeqCst);
        if before <= 1 {
            self.flush();
        }
    }

    pub fn flush(&self) {
        let waiting: Vec<String> = match self.queued.lock() {
            Ok(mut queued) => std::mem::take(&mut *queued),
            Err(_) => return,
        };

        if self
            .dropped
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.say(HELD_BACK.to_string());
        }

        for line in waiting {
            self.say(line);
        }
    }

    fn holding(&self) -> bool {
        self.held.load(std::sync::atomic::Ordering::SeqCst) > 0
    }

    fn say(&self, text: String) {
        let current = match self.inner.read() {
            Ok(current) => Arc::clone(&current),
            Err(_) => return,
        };
        current.notify(text);
    }
}

impl Notifier for SharedNotifier {
    fn notify(&self, text: String) {
        if !self.holding() {
            self.say(text);
            return;
        }

        let Ok(mut queued) = self.queued.lock() else {
            return;
        };

        // Something shouting must not grow the queue without end. Keep the
        // newest, drop the oldest, and say once that some were dropped — the
        // one thing worse than late news is news that vanished quietly.
        while queued.len() >= QUEUE_LIMIT {
            queued.remove(0);
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }

        queued.push(text);
    }
}

const HELD_BACK: &str = "  · some earlier background lines were dropped, too many at once";

struct DetachedUi {
    notifier: Arc<dyn Notifier>,
    label: String,
}

pub fn detached_ui() -> impl UserInterface {
    DetachedUi {
        notifier: Arc::new(SilentNotifier),
        label: "inbox".to_string(),
    }
}

pub fn subagent_ui(notifier: Arc<dyn Notifier>, agent_id: &str) -> impl UserInterface {
    DetachedUi {
        notifier,
        label: format!("subagent {}", &agent_id[..agent_id.len().min(8)]),
    }
}

struct SilentNotifier;

impl Notifier for SilentNotifier {
    fn notify(&self, _text: String) {}
}

impl DetachedUi {
    fn line(&self, marker: &str, text: &str) {
        self.notifier
            .notify(format!("  {} {} · {}", marker, self.label, text));
    }
}

#[async_trait::async_trait]
impl UserInterface for DetachedUi {
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
            "asked a question with {} option(s) with nobody at the keyboard",
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

        let mut ui = DetachedUi {
            notifier: Arc::clone(&self.notifier),
            label: format!("job {} · {}", job.id, job.name),
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

    fn ui(collector: Arc<Collector>) -> DetachedUi {
        DetachedUi {
            notifier: collector,
            label: "job 7 · watcher".to_string(),
        }
    }

    #[test]
    fn nothing_reaches_the_screen_while_the_gate_is_held() {
        let collector = Arc::new(Collector {
            lines: StdMutex::new(Vec::new()),
        });
        let shared = SharedNotifier::new(Arc::clone(&collector) as Arc<dyn Notifier>);

        shared.hold();
        shared.notify("a job spoke".to_string());
        shared.notify("and again".to_string());

        assert!(
            collector.lines.lock().unwrap().is_empty(),
            "a background line landed in the middle of an answer"
        );

        shared.release();
        let said = collector.lines.lock().unwrap();
        assert_eq!(
            said.as_slice(),
            ["a job spoke".to_string(), "and again".to_string()],
            "the queue came out in the wrong order"
        );
    }

    #[test]
    fn a_turn_inside_a_turn_does_not_open_the_gate_early() {
        let collector = Arc::new(Collector {
            lines: StdMutex::new(Vec::new()),
        });
        let shared = SharedNotifier::new(Arc::clone(&collector) as Arc<dyn Notifier>);

        shared.hold();
        shared.hold();
        shared.notify("waiting".to_string());
        shared.release();

        assert!(
            collector.lines.lock().unwrap().is_empty(),
            "the inner release let it out while the outer turn was still going"
        );

        shared.release();
        assert_eq!(collector.lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn an_open_gate_passes_everything_straight_through() {
        let collector = Arc::new(Collector {
            lines: StdMutex::new(Vec::new()),
        });
        let shared = SharedNotifier::new(Arc::clone(&collector) as Arc<dyn Notifier>);

        shared.notify("idle chatter".to_string());
        assert_eq!(collector.lines.lock().unwrap().len(), 1);
    }

    #[test]
    fn a_flood_is_capped_and_says_that_it_was() {
        let collector = Arc::new(Collector {
            lines: StdMutex::new(Vec::new()),
        });
        let shared = SharedNotifier::new(Arc::clone(&collector) as Arc<dyn Notifier>);

        shared.hold();
        for n in 0..QUEUE_LIMIT + 50 {
            shared.notify(format!("line {n}"));
        }
        shared.release();

        let said = collector.lines.lock().unwrap();
        assert!(said.len() <= QUEUE_LIMIT + 1, "the queue grew without end");
        assert_eq!(said.first().map(|l| l.as_str()), Some(HELD_BACK));
        assert!(
            said.last()
                .unwrap()
                .contains(&format!("line {}", QUEUE_LIMIT + 49)),
            "the newest line was dropped instead of the oldest"
        );
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
