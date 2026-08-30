use std::path::Path;

use chrono::Utc;
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use tokio::sync::Mutex;

const BUSY_TIMEOUT_MS: u64 = 5_000;

pub const EVERYONE: &str = "everyone";

use crate::error::JumabekResult;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Task,
    Finding,
    Decision,
    Question,
}

impl Kind {
    pub fn parse(raw: &str) -> Option<Kind> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "task" | "todo" => Some(Kind::Task),
            "finding" | "result" => Some(Kind::Finding),
            "decision" | "conclusion" => Some(Kind::Decision),
            "question" | "ask" => Some(Kind::Question),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Task => "task",
            Kind::Finding => "finding",
            Kind::Decision => "decision",
            Kind::Question => "question",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum EntryState {
    Open,
    Claimed,
    Done,
}

impl EntryState {
    pub fn parse(raw: &str) -> Option<EntryState> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "open" => Some(EntryState::Open),
            "claimed" | "claim" | "mine" => Some(EntryState::Claimed),
            "done" | "closed" | "finished" => Some(EntryState::Done),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            EntryState::Open => "open",
            EntryState::Claimed => "claimed",
            EntryState::Done => "done",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub id: i64,
    pub group_id: String,
    pub author: String,
    pub addressee: String,
    pub kind: Kind,
    pub body: String,
    pub state: EntryState,
    pub created_at: String,
}

impl Entry {
    pub fn line(&self) -> String {
        format!(
            "#{} {} · {} · from {} to {}\n  {}",
            self.id,
            self.kind.as_str(),
            self.state.as_str(),
            short(&self.author),
            short(&self.addressee),
            self.body
        )
    }

    pub fn meant_for(&self, agent_id: &str, role: Option<&str>) -> bool {
        self.addressee == EVERYONE
            || self.addressee == agent_id
            || role.is_some_and(|name| self.addressee == name)
    }
}

fn short(id: &str) -> String {
    if id == EVERYONE || id.len() <= 8 {
        return id.to_string();
    }
    id.chars().take(8).collect()
}

#[derive(Debug, Clone)]
pub struct Group {
    pub id: String,
    pub goal: String,
    pub budget: u32,
    pub spent: u32,
}

impl Group {
    pub fn left(&self) -> u32 {
        self.budget.saturating_sub(self.spent)
    }

    pub fn exhausted(&self) -> bool {
        self.spent >= self.budget
    }
}

pub struct Board {
    conn: Mutex<Connection>,
}

impl Board {
    pub fn open(path: &Path) -> JumabekResult<Self> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(std::time::Duration::from_millis(BUSY_TIMEOUT_MS))?;
        Ok(Board {
            conn: Mutex::new(conn),
        })
    }

    pub async fn open_group(&self, id: &str, goal: &str, budget: u32) -> JumabekResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT OR IGNORE INTO agent_groups (id, goal, budget, spent, state, created_at)
             VALUES (?1, ?2, ?3, 0, 'open', ?4)",
            params![id, goal, budget, Utc::now().to_rfc3339()],
        )?;
        Ok(())
    }

    pub async fn group(&self, id: &str) -> JumabekResult<Option<Group>> {
        let conn = self.conn.lock().await;
        let mut stmt =
            conn.prepare("SELECT id, goal, budget, spent FROM agent_groups WHERE id = ?1")?;

        let mut rows = stmt.query_map(params![id], |row| {
            Ok(Group {
                id: row.get(0)?,
                goal: row.get(1)?,
                budget: row.get::<_, i64>(2)? as u32,
                spent: row.get::<_, i64>(3)? as u32,
            })
        })?;

        match rows.next() {
            Some(group) => Ok(Some(group?)),
            None => Ok(None),
        }
    }

    pub async fn spend(&self, id: &str) -> JumabekResult<Option<Group>> {
        {
            let conn = self.conn.lock().await;
            conn.execute(
                "UPDATE agent_groups SET spent = spent + 1 WHERE id = ?1",
                params![id],
            )?;
        }
        self.group(id).await
    }

    pub async fn close_group(&self, id: &str) -> JumabekResult<bool> {
        let conn = self.conn.lock().await;
        let changed = conn.execute(
            "UPDATE agent_groups SET state = 'closed' WHERE id = ?1 AND state != 'closed'",
            params![id],
        )?;
        Ok(changed > 0)
    }

    pub async fn post(
        &self,
        group_id: &str,
        author: &str,
        addressee: &str,
        kind: Kind,
        body: &str,
    ) -> JumabekResult<i64> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO board (group_id, author, addressee, kind, body, state, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, 'open', ?6)",
            params![
                group_id,
                author,
                addressee,
                kind.as_str(),
                body,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(conn.last_insert_rowid())
    }

    pub async fn entries(&self, group_id: &str) -> JumabekResult<Vec<Entry>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, group_id, author, addressee, kind, body, state, created_at
               FROM board
              WHERE group_id = ?1
              ORDER BY id",
        )?;

        let rows = stmt
            .query_map(params![group_id], |row| {
                Ok(Entry {
                    id: row.get(0)?,
                    group_id: row.get(1)?,
                    author: row.get(2)?,
                    addressee: row.get(3)?,
                    kind: Kind::parse(&row.get::<_, String>(4)?).unwrap_or(Kind::Finding),
                    body: row.get(5)?,
                    state: EntryState::parse(&row.get::<_, String>(6)?).unwrap_or(EntryState::Open),
                    created_at: row.get(7)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub async fn set_state(
        &self,
        group_id: &str,
        id: i64,
        state: EntryState,
    ) -> JumabekResult<bool> {
        let conn = self.conn.lock().await;
        let changed = conn.execute(
            "UPDATE board SET state = ?1 WHERE id = ?2 AND group_id = ?3",
            params![state.as_str(), id, group_id],
        )?;
        Ok(changed > 0)
    }

    pub async fn audit(
        &self,
        asked_by: &str,
        wanted: &str,
        why: &str,
        verdict: &str,
        decided_by: &str,
    ) -> JumabekResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO grant_audit (asked_by, wanted, why, verdict, decided_by, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                asked_by,
                wanted,
                why,
                verdict,
                decided_by,
                Utc::now().to_rfc3339()
            ],
        )?;
        Ok(())
    }

    pub async fn expansions(&self) -> JumabekResult<Vec<String>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT created_at, asked_by, wanted, verdict, decided_by, why
               FROM grant_audit ORDER BY id",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok(format!(
                    "{} · {} wanted {} · {} by {} · {}",
                    row.get::<_, String>(0)?,
                    short(&row.get::<_, String>(1)?),
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, String>(5)?
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }
}

pub fn as_text(entries: &[Entry], reader: &str, role: Option<&str>) -> String {
    if entries.is_empty() {
        return "the board is empty".to_string();
    }

    entries
        .iter()
        .map(|entry| {
            if entry.meant_for(reader, role) {
                format!("-> {}", entry.line())
            } else {
                format!("   {}", entry.line())
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn board() -> Board {
        let file = std::env::temp_dir().join(format!("board-{}.db", uuid::Uuid::new_v4()));
        let conn = Connection::open(&file).expect("open");
        conn.execute_batch(crate::memory::schema::SCHEMA)
            .expect("schema");
        drop(conn);
        Board::open(&file).expect("board")
    }

    #[tokio::test]
    async fn an_entry_is_only_visible_inside_its_own_group() {
        let board = board().await;
        board.open_group("g1", "find the leak", 30).await.unwrap();
        board
            .open_group("g2", "write the report", 30)
            .await
            .unwrap();

        board
            .post("g1", "a", EVERYONE, Kind::Finding, "the leak is in parse()")
            .await
            .unwrap();

        assert_eq!(board.entries("g1").await.unwrap().len(), 1);
        assert!(
            board.entries("g2").await.unwrap().is_empty(),
            "another group could read the board"
        );
    }

    #[tokio::test]
    async fn a_shared_budget_runs_out_however_many_agents_spend_it() {
        let board = board().await;
        board.open_group("g", "goal", 3).await.unwrap();

        for _ in 0..3 {
            let group = board.spend("g").await.unwrap().unwrap();
            assert!(group.spent <= group.budget);
        }

        let group = board.group("g").await.unwrap().unwrap();
        assert!(group.exhausted(), "the group budget never ran out");
        assert_eq!(group.left(), 0);
    }

    #[tokio::test]
    async fn opening_a_group_twice_does_not_reset_what_it_has_spent() {
        let board = board().await;
        board.open_group("g", "goal", 10).await.unwrap();
        board.spend("g").await.unwrap();
        board.open_group("g", "goal", 10).await.unwrap();

        assert_eq!(board.group("g").await.unwrap().unwrap().spent, 1);
    }

    #[tokio::test]
    async fn only_the_first_agent_to_notice_closes_the_group() {
        let board = board().await;
        board.open_group("g", "goal", 1).await.unwrap();

        assert!(board.close_group("g").await.unwrap());
        assert!(
            !board.close_group("g").await.unwrap(),
            "a second agent closed an already closed group and would write the record twice"
        );
    }

    #[tokio::test]
    async fn an_entry_can_be_claimed_so_two_agents_do_not_do_it_twice() {
        let board = board().await;
        board.open_group("g", "goal", 10).await.unwrap();
        let id = board
            .post("g", "a", EVERYONE, Kind::Task, "read the logs")
            .await
            .unwrap();

        assert!(board.set_state("g", id, EntryState::Claimed).await.unwrap());
        assert_eq!(
            board.entries("g").await.unwrap()[0].state,
            EntryState::Claimed
        );
    }

    #[tokio::test]
    async fn an_entry_cannot_be_touched_from_another_group() {
        let board = board().await;
        board.open_group("g", "goal", 10).await.unwrap();
        board.open_group("other", "goal", 10).await.unwrap();
        let id = board
            .post("g", "a", EVERYONE, Kind::Task, "read the logs")
            .await
            .unwrap();

        assert!(
            !board
                .set_state("other", id, EntryState::Done)
                .await
                .unwrap(),
            "a stranger closed another group's entry"
        );
    }

    #[tokio::test]
    async fn every_expansion_leaves_a_record_naming_who_decided() {
        let board = board().await;
        board
            .audit(
                "agent-a",
                "shell_executor",
                "needs to read a log",
                "granted",
                "user",
            )
            .await
            .unwrap();

        let written = board.expansions().await.unwrap();
        assert_eq!(written.len(), 1);
        assert!(written[0].contains("granted by user"), "{:?}", written);
        assert!(written[0].contains("shell_executor"));
    }

    #[test]
    fn an_entry_addressed_to_everyone_reaches_everyone() {
        let entry = Entry {
            id: 1,
            group_id: "g".to_string(),
            author: "a".to_string(),
            addressee: EVERYONE.to_string(),
            kind: Kind::Task,
            body: "x".to_string(),
            state: EntryState::Open,
            created_at: String::new(),
        };

        assert!(entry.meant_for("anyone", None));
    }

    #[test]
    fn an_entry_addressed_to_a_role_reaches_whoever_holds_it() {
        let entry = Entry {
            id: 1,
            group_id: "g".to_string(),
            author: "a".to_string(),
            addressee: "researcher".to_string(),
            kind: Kind::Task,
            body: "x".to_string(),
            state: EntryState::Open,
            created_at: String::new(),
        };

        assert!(entry.meant_for("someone", Some("researcher")));
        assert!(!entry.meant_for("someone", Some("writer")));
        assert!(!entry.meant_for("someone", None));
    }

    #[test]
    fn a_kind_the_model_invented_is_refused_rather_than_guessed() {
        assert_eq!(Kind::parse("finding"), Some(Kind::Finding));
        assert_eq!(Kind::parse("  DECISION "), Some(Kind::Decision));
        assert_eq!(Kind::parse("gossip"), None);
    }

    #[test]
    fn an_empty_board_says_so() {
        assert_eq!(as_text(&[], "me", None), "the board is empty");
    }
}
