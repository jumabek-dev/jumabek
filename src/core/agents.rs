use std::collections::HashMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

const KEPT_AFTER_END_SEC: i64 = 60;
const TASK_SHOWN: usize = 90;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    AwaitingPermission,
    Finished,
    Failed,
}

impl State {
    pub fn id(self) -> &'static str {
        match self {
            State::Running => "running",
            State::AwaitingPermission => "awaiting_permission",
            State::Finished => "finished",
            State::Failed => "failed",
        }
    }

    pub fn is_over(self) -> bool {
        matches!(self, State::Finished | State::Failed)
    }
}

impl std::fmt::Display for State {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.id())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentEntry {
    pub agent_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub role: Option<String>,
    pub task: String,
    pub depth: u32,
    pub iteration: u32,
    pub max_iterations: u32,
    pub doing: String,
    pub state: State,
    pub started_at: DateTime<Utc>,
    pub seconds: i64,
    #[serde(skip)]
    ended_at: Option<DateTime<Utc>>,
}

impl AgentEntry {
    pub fn new(agent_id: impl Into<String>, task: impl Into<String>) -> Self {
        AgentEntry {
            agent_id: agent_id.into(),
            parent_id: None,
            group_id: None,
            role: None,
            task: task.into(),
            depth: 0,
            iteration: 0,
            max_iterations: 0,
            doing: "starting".to_string(),
            state: State::Running,
            started_at: Utc::now(),
            seconds: 0,
            ended_at: None,
        }
    }

    pub fn under(mut self, parent_id: Option<String>, depth: u32) -> Self {
        self.parent_id = parent_id;
        self.depth = depth;
        self
    }

    pub fn allowed(mut self, iterations: u32) -> Self {
        self.max_iterations = iterations;
        self
    }

    pub fn short_id(&self) -> &str {
        let cut = self
            .agent_id
            .char_indices()
            .nth(8)
            .map(|(at, _)| at)
            .unwrap_or(self.agent_id.len());
        &self.agent_id[..cut]
    }

    pub fn line(&self) -> String {
        format!(
            "agent {} · {} · iteration {}/{} · {}s\n  task: {}\n  doing: {}",
            self.agent_id,
            self.state,
            self.iteration,
            self.max_iterations,
            self.seconds,
            shorten(&self.task, TASK_SHOWN),
            self.doing
        )
    }

    fn aged(&self, now: DateTime<Utc>) -> AgentEntry {
        let until = self.ended_at.unwrap_or(now);
        let mut copy = self.clone();
        copy.seconds = (until - self.started_at).num_seconds().max(0);
        copy
    }

    fn outstayed(&self, now: DateTime<Utc>) -> bool {
        match self.ended_at {
            Some(end) => (now - end).num_seconds() > KEPT_AFTER_END_SEC,
            None => false,
        }
    }
}

fn shorten(text: &str, limit: usize) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match flat.char_indices().nth(limit) {
        Some((at, _)) => format!("{}…", &flat[..at]),
        None => flat,
    }
}

#[derive(Debug, Default)]
pub struct AgentRegistry {
    entries: RwLock<HashMap<String, AgentEntry>>,
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, entry: AgentEntry) {
        let now = Utc::now();
        let mut entries = self.entries.write().await;
        entries.retain(|_, kept| !kept.outstayed(now));
        entries.insert(entry.agent_id.clone(), entry);
    }

    pub async fn doing(&self, agent_id: &str, what: &str) {
        self.amend(agent_id, |entry| what.clone_into(&mut entry.doing))
            .await;
    }

    pub async fn iteration(&self, agent_id: &str, reached: u32) {
        self.amend(agent_id, |entry| entry.iteration = reached)
            .await;
    }

    pub async fn waiting(&self, agent_id: &str, on_the_user: bool) {
        self.amend(agent_id, |entry| {
            if !entry.state.is_over() {
                entry.state = if on_the_user {
                    State::AwaitingPermission
                } else {
                    State::Running
                };
            }
        })
        .await;
    }

    pub async fn finished(&self, agent_id: &str, state: State) {
        self.amend(agent_id, |entry| {
            entry.state = state;
            entry.ended_at = Some(Utc::now());
        })
        .await;
    }

    pub async fn snapshot(&self) -> Vec<AgentEntry> {
        let now = Utc::now();
        let entries = self.entries.read().await;
        let mut all: Vec<AgentEntry> = entries
            .values()
            .filter(|entry| !entry.outstayed(now))
            .map(|entry| entry.aged(now))
            .collect();
        all.sort_by_key(|entry| entry.started_at);
        all
    }

    pub async fn running(&self) -> Vec<AgentEntry> {
        self.snapshot()
            .await
            .into_iter()
            .filter(|entry| !entry.state.is_over())
            .collect()
    }

    pub async fn others(&self, than: &str) -> Vec<AgentEntry> {
        self.running()
            .await
            .into_iter()
            .filter(|entry| entry.agent_id != than)
            .collect()
    }

    async fn amend(&self, agent_id: &str, change: impl FnOnce(&mut AgentEntry)) {
        if let Some(entry) = self.entries.write().await.get_mut(agent_id) {
            change(entry);
        }
    }
}

pub fn as_json(entries: &[AgentEntry]) -> String {
    serde_json::json!({ "agents": entries }).to_string()
}

pub fn as_text(entries: &[AgentEntry]) -> String {
    if entries.is_empty() {
        return "no agents are running".to_string();
    }

    entries
        .iter()
        .map(AgentEntry::line)
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str) -> AgentEntry {
        AgentEntry::new(id, "do the thing").allowed(10)
    }

    #[tokio::test]
    async fn an_agent_appears_while_it_runs_and_reports_what_it_is_doing() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;
        registry.doing("a", "skill · shell_executor.run").await;
        registry.iteration("a", 3).await;

        let running = registry.running().await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].doing, "skill · shell_executor.run");
        assert_eq!(running[0].iteration, 3);
        assert_eq!(running[0].state, State::Running);
    }

    #[tokio::test]
    async fn a_finished_agent_drops_out_of_the_running_list() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;
        registry.register(entry("b")).await;
        registry.finished("a", State::Finished).await;

        let running = registry.running().await;
        assert_eq!(running.len(), 1, "a finished agent was still listed");
        assert_eq!(running[0].agent_id, "b");

        assert_eq!(
            registry.snapshot().await.len(),
            2,
            "the snapshot should still show what just ended"
        );
    }

    #[tokio::test]
    async fn a_failed_agent_says_so_rather_than_vanishing() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;
        registry.finished("a", State::Failed).await;

        let snapshot = registry.snapshot().await;
        assert_eq!(snapshot[0].state, State::Failed);
        assert!(registry.running().await.is_empty());
    }

    #[tokio::test]
    async fn waiting_on_the_user_is_visible_and_reversible() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;

        registry.waiting("a", true).await;
        assert_eq!(registry.running().await[0].state, State::AwaitingPermission);

        registry.waiting("a", false).await;
        assert_eq!(registry.running().await[0].state, State::Running);
    }

    #[tokio::test]
    async fn an_agent_that_is_over_is_not_dragged_back_to_running() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;
        registry.finished("a", State::Finished).await;
        registry.waiting("a", false).await;

        assert_eq!(registry.snapshot().await[0].state, State::Finished);
    }

    #[tokio::test]
    async fn an_agent_that_ended_long_enough_ago_stops_being_reported_at_all() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;
        registry.finished("a", State::Finished).await;

        registry
            .amend("a", |entry| {
                entry.ended_at =
                    Some(Utc::now() - chrono::Duration::seconds(KEPT_AFTER_END_SEC + 1))
            })
            .await;

        assert!(
            registry.snapshot().await.is_empty(),
            "a long-finished agent was still being reported"
        );
    }

    #[tokio::test]
    async fn the_clock_stops_when_an_agent_does() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;

        registry
            .amend("a", |entry| {
                entry.started_at = Utc::now() - chrono::Duration::seconds(30);
                entry.ended_at = Some(Utc::now() - chrono::Duration::seconds(10));
                entry.state = State::Finished;
            })
            .await;

        assert_eq!(
            registry.snapshot().await[0].seconds,
            20,
            "a finished agent should report how long it took, not how long ago it was"
        );
    }

    #[tokio::test]
    async fn a_running_agent_reports_how_long_it_has_been_going() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;
        registry
            .amend("a", |entry| {
                entry.started_at = Utc::now() - chrono::Duration::seconds(7)
            })
            .await;

        assert_eq!(registry.snapshot().await[0].seconds, 7);
    }

    #[tokio::test]
    async fn updates_to_an_agent_that_is_gone_are_ignored_rather_than_panicking() {
        let registry = AgentRegistry::new();
        registry.doing("nobody", "something").await;
        registry.iteration("nobody", 4).await;
        registry.finished("nobody", State::Failed).await;

        assert!(registry.snapshot().await.is_empty());
    }

    #[tokio::test]
    async fn many_agents_registering_and_leaving_at_once_all_land() {
        let registry = std::sync::Arc::new(AgentRegistry::new());
        let mut handles = Vec::new();

        for n in 0..50 {
            let registry = std::sync::Arc::clone(&registry);
            handles.push(tokio::spawn(async move {
                let id = format!("agent-{n}");
                registry.register(entry(&id)).await;
                registry.doing(&id, "working").await;
                if n % 2 == 0 {
                    registry.finished(&id, State::Finished).await;
                }
            }));
        }

        for handle in handles {
            handle.await.expect("a registry writer panicked");
        }

        assert_eq!(registry.snapshot().await.len(), 50);
        assert_eq!(registry.running().await.len(), 25);
    }

    #[tokio::test]
    async fn re_running_the_same_agent_replaces_its_old_entry() {
        let registry = AgentRegistry::new();
        registry.register(entry("a")).await;
        registry.finished("a", State::Finished).await;
        registry
            .register(AgentEntry::new("a", "the next thing").allowed(10))
            .await;

        let running = registry.running().await;
        assert_eq!(running.len(), 1);
        assert_eq!(running[0].task, "the next thing");
    }

    #[tokio::test]
    async fn an_agent_asking_who_is_running_is_never_in_its_own_answer() {
        let registry = AgentRegistry::new();
        registry.register(entry("me")).await;
        registry.register(entry("child")).await;
        registry.register(entry("done")).await;
        registry.finished("done", State::Finished).await;

        let seen = registry.others("me").await;
        let ids: Vec<&str> = seen.iter().map(|e| e.agent_id.as_str()).collect();
        assert_eq!(ids, vec!["child"], "asked for others, got {ids:?}");
    }

    #[tokio::test]
    async fn a_child_records_who_spawned_it() {
        let registry = AgentRegistry::new();
        registry
            .register(entry("child").under(Some("parent".to_string()), 1))
            .await;

        let entry = &registry.running().await[0];
        assert_eq!(entry.parent_id.as_deref(), Some("parent"));
        assert_eq!(entry.depth, 1);
    }

    #[test]
    fn an_entry_reads_as_something_a_person_could_scan() {
        let mut entry = entry("1f2a3b4c-dead-beef");
        entry.iteration = 2;
        entry.doing = "thinking".to_string();

        let line = entry.line();
        assert!(line.contains("iteration 2/10"), "{line}");
        assert!(line.contains("running"), "{line}");
        assert!(line.contains("do the thing"), "{line}");
    }

    #[test]
    fn a_long_task_is_cut_down_and_flattened_to_one_line() {
        let entry = AgentEntry::new("a", format!("first line\n{}", "x".repeat(200)));
        let line = entry.line();
        assert_eq!(line.lines().count(), 3, "{line}");
        assert!(line.contains('…'));
    }

    #[test]
    fn nothing_running_says_so_instead_of_returning_an_empty_string() {
        assert_eq!(as_text(&[]), "no agents are running");
    }

    #[tokio::test]
    async fn the_endpoint_answers_with_valid_json_when_nothing_is_running() {
        let registry = AgentRegistry::new();
        let body = as_json(&registry.snapshot().await);

        assert_eq!(body, r#"{"agents":[]}"#);
        serde_json::from_str::<serde_json::Value>(&body).expect("the body was not JSON");
    }

    #[test]
    fn an_entry_survives_the_trip_out_to_a_watcher_and_back() {
        let mut sent = entry("1f2a3b4c-dead-beef");
        sent.doing = "skill · shell_executor.run".to_string();
        sent.iteration = 4;
        sent.seconds = 12;
        sent.state = State::AwaitingPermission;
        sent.parent_id = Some("parent".to_string());

        let body = as_json(std::slice::from_ref(&sent));
        let back: serde_json::Value = serde_json::from_str(&body).expect("not JSON");
        let one = &back["agents"][0];
        let read: AgentEntry = serde_json::from_value(one.clone()).expect("unreadable entry");

        assert_eq!(read.agent_id, sent.agent_id);
        assert_eq!(read.doing, sent.doing);
        assert_eq!(read.state, State::AwaitingPermission);
        assert_eq!(read.iteration, 4);
        assert_eq!(read.seconds, 12);
        assert_eq!(read.parent_id.as_deref(), Some("parent"));
        assert_eq!(read.started_at, sent.started_at);
    }

    #[test]
    fn a_watcher_can_read_an_entry_that_carries_none_of_the_optional_fields() {
        let read: AgentEntry = serde_json::from_str(
            r#"{"agent_id":"a","task":"t","depth":0,"iteration":0,"max_iterations":10,
                "doing":"thinking","state":"running","started_at":"2026-08-30T10:00:00Z","seconds":0}"#,
        )
        .expect("a minimal entry should still read");

        assert!(read.parent_id.is_none());
        assert!(read.group_id.is_none());
        assert!(read.role.is_none());
    }

    #[test]
    fn a_short_id_is_safe_on_an_id_shorter_than_the_cut() {
        assert_eq!(AgentEntry::new("abc", "t").short_id(), "abc");
        assert_eq!(
            AgentEntry::new("0123456789abcdef", "t").short_id(),
            "01234567"
        );
    }
}
