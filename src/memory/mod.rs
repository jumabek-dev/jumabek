pub mod facts;
pub mod query;
pub mod retrieval;
pub mod schema;

use std::path::Path;

use chrono::Utc;
use rusqlite::{Connection, params};
use tokio::sync::Mutex;

use crate::error::{JumabekError, JumabekResult};

const EMBED_BATCH: usize = 64;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Skill,
    System,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Skill => "skill",
            Role::System => "system",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NewMessage {
    pub role: Role,
    pub content: String,
    pub task_id: Option<String>,
    pub parent_task_id: Option<String>,
    pub raw_json: Option<String>,
    pub level: Option<String>,
    pub level_change: Option<String>,
}

impl NewMessage {
    pub fn new(role: Role, content: impl Into<String>) -> Self {
        NewMessage {
            role,
            content: content.into(),
            task_id: None,
            parent_task_id: None,
            raw_json: None,
            level: None,
            level_change: None,
        }
    }

    pub fn task(mut self, task_id: impl Into<String>) -> Self {
        self.task_id = Some(task_id.into());
        self
    }

    pub fn parent(mut self, parent_task_id: Option<String>) -> Self {
        self.parent_task_id = parent_task_id;
        self
    }

    pub fn raw(mut self, raw_json: impl Into<String>) -> Self {
        self.raw_json = Some(raw_json.into());
        self
    }

    pub fn level(mut self, level: Option<impl Into<String>>) -> Self {
        self.level = level.map(Into::into);
        self
    }

    pub fn level_change(mut self, reason: Option<impl Into<String>>) -> Self {
        self.level_change = reason.map(Into::into);
        self
    }
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: i64,
    pub task_id: Option<String>,
    pub role: String,
    pub content: String,
    pub raw_json: Option<String>,
}

impl StoredMessage {
    pub fn llm_content(&self) -> &str {
        self.raw_json.as_deref().unwrap_or(&self.content)
    }
}

#[derive(Debug, Clone)]
pub struct MemoryHit {
    pub session_id: i64,
    pub created_at: String,
    pub role: String,
    pub content: String,
}

#[derive(Debug, Clone)]
pub struct Spending {
    pub model: String,
    pub protocol: String,
    pub turns: u32,
    pub counted_in: u32,
    pub counted_out: u32,
    pub cache_read: u32,
    pub cache_write: u32,
    pub turns_that_reported_caching: u32,
    pub guessed_in: u32,
}

pub struct Memory {
    conn: Mutex<Connection>,
    session_id: i64,
    embedder: Option<std::sync::Arc<retrieval::Embedder>>,
}

impl Memory {
    pub async fn open(path: &Path, interface: &str) -> JumabekResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let conn = Connection::open(path)?;
        migrate(&conn)?;
        conn.execute_batch(schema::SCHEMA)?;

        let started_at = Utc::now().to_rfc3339();
        conn.execute(
            "INSERT INTO sessions (started_at, interface) VALUES (?1, ?2)",
            params![started_at, interface],
        )?;
        let session_id = conn.last_insert_rowid();

        Ok(Memory {
            conn: Mutex::new(conn),
            session_id,
            embedder: None,
        })
    }

    pub async fn start_retrieval(&mut self) -> JumabekResult<usize> {
        let embedder = std::sync::Arc::new(retrieval::Embedder::open()?);
        self.embedder = Some(std::sync::Arc::clone(&embedder));
        self.catch_up(&embedder).await
    }

    async fn catch_up(&self, embedder: &retrieval::Embedder) -> JumabekResult<usize> {
        let mut done = 0;

        loop {
            let waiting = {
                let conn = self.conn.lock().await;
                facts::awaiting_vectors(&conn, EMBED_BATCH)?
            };

            if waiting.is_empty() {
                return Ok(done);
            }

            let texts: Vec<String> = waiting.iter().map(|(_, text)| text.clone()).collect();
            let vectors = embedder.embed(texts)?;

            if vectors.len() != waiting.len() {
                return Err(JumabekError::InternalError(format!(
                    "asked for {} vectors and got {}",
                    waiting.len(),
                    vectors.len()
                )));
            }

            let conn = self.conn.lock().await;
            for ((id, _), vector) in waiting.iter().zip(vectors) {
                facts::set_vector(&conn, *id, &retrieval::to_blob(&vector))?;
            }

            done += waiting.len();
        }
    }

    pub async fn facts_for(
        &self,
        about: &str,
        project: Option<&str>,
    ) -> JumabekResult<Vec<facts::Fact>> {
        let Some(embedder) = &self.embedder else {
            return self.known_facts().await;
        };

        if about.trim().is_empty() {
            return self.known_facts().await;
        }

        let mut query = embedder.embed(vec![about.to_string()])?;
        let Some(query) = query.pop() else {
            return self.known_facts().await;
        };

        let candidates = {
            let conn = self.conn.lock().await;
            facts::with_vectors(&conn, facts::LOCAL)?
        };

        Ok(retrieval::choose(
            candidates,
            &query,
            project,
            retrieval::KEPT,
        ))
    }

    pub fn session_id(&self) -> i64 {
        self.session_id
    }

    pub async fn log(&self, message: NewMessage) -> JumabekResult<i64> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO messages
                 (session_id, task_id, parent_task_id, role, content, raw_json, level,
                  level_change, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                self.session_id,
                message.task_id,
                message.parent_task_id,
                message.role.as_str(),
                message.content,
                message.raw_json,
                message.level,
                message.level_change,
                Utc::now().to_rfc3339(),
            ],
        )?;

        let id = conn.last_insert_rowid();

        if matches!(message.role, Role::User | Role::Assistant) {
            let searchable = query::to_search_text(&message.content);
            if !searchable.is_empty() {
                conn.execute(
                    "INSERT INTO messages_fts(rowid, content) VALUES (?1, ?2)",
                    params![id, searchable],
                )?;
            }
        }

        Ok(id)
    }

    pub async fn current_session(&self) -> JumabekResult<Vec<StoredMessage>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT id, task_id, role, content, raw_json
               FROM messages
              WHERE session_id = ?1
              ORDER BY id",
        )?;

        let rows = stmt
            .query_map(params![self.session_id], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    raw_json: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub async fn previous_session_tail(&self, limit: u32) -> JumabekResult<Vec<StoredMessage>> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        let conn = self.conn.lock().await;

        let previous: Option<i64> = conn
            .query_row(
                "SELECT MAX(id) FROM sessions WHERE id < ?1",
                params![self.session_id],
                |row| row.get(0),
            )
            .ok()
            .flatten();

        let Some(previous) = previous else {
            return Ok(Vec::new());
        };

        let mut stmt = conn.prepare(
            "SELECT id, task_id, role, content, raw_json
               FROM messages
              WHERE session_id = ?1
              ORDER BY id DESC
              LIMIT ?2",
        )?;

        let mut rows = stmt
            .query_map(params![previous, limit], |row| {
                Ok(StoredMessage {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                    raw_json: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        rows.reverse();
        Ok(rows)
    }

    pub async fn log_usage(
        &self,
        task_id: &str,
        model: &str,
        protocol: &str,
        usage: &crate::core::usage::Usage,
        guessed_in: u32,
    ) -> JumabekResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "INSERT INTO token_usage (session_id, task_id, model, protocol, counted_in,
                                      counted_out, cache_read, cache_write, guessed_in, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
            params![
                self.session_id,
                task_id,
                model,
                protocol,
                usage.billed_input(),
                usage.output,
                usage.cache_read,
                usage.cache_write,
                guessed_in,
                Utc::now().to_rfc3339(),
            ],
        )?;
        Ok(())
    }

    pub async fn spending(&self) -> JumabekResult<Vec<Spending>> {
        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT model, protocol, count(*), sum(counted_in), sum(counted_out),
                    sum(coalesce(cache_read, 0)), sum(coalesce(cache_write, 0)),
                    sum(cache_read IS NOT NULL), sum(guessed_in)
               FROM token_usage
              GROUP BY model, protocol
              ORDER BY sum(counted_in) DESC",
        )?;

        let rows = stmt
            .query_map([], |row| {
                Ok(Spending {
                    model: row.get(0)?,
                    protocol: row.get(1)?,
                    turns: row.get::<_, i64>(2)? as u32,
                    counted_in: row.get::<_, i64>(3)? as u32,
                    counted_out: row.get::<_, i64>(4)? as u32,
                    cache_read: row.get::<_, i64>(5)? as u32,
                    cache_write: row.get::<_, i64>(6)? as u32,
                    turns_that_reported_caching: row.get::<_, i64>(7)? as u32,
                    guessed_in: row.get::<_, i64>(8)? as u32,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub async fn remember(&self, fact: &facts::Fact, also: bool) -> JumabekResult<facts::Written> {
        let conn = self.conn.lock().await;
        facts::remember(&conn, fact, also)
    }

    pub async fn forget(&self, subject: &str, key: Option<&str>) -> JumabekResult<usize> {
        let conn = self.conn.lock().await;
        facts::forget(&conn, subject, key)
    }

    pub async fn known_facts(&self) -> JumabekResult<Vec<facts::Fact>> {
        let conn = self.conn.lock().await;
        facts::all(&conn)
    }

    pub async fn search(&self, raw_query: &str, limit: u32) -> JumabekResult<Vec<MemoryHit>> {
        let Some(match_query) = query::build_match_query(raw_query) else {
            return Ok(Vec::new());
        };

        let conn = self.conn.lock().await;
        let mut stmt = conn.prepare(
            "SELECT m.session_id, m.created_at, m.role, m.content
               FROM messages_fts f
               JOIN messages m ON m.id = f.rowid
              WHERE messages_fts MATCH ?1
                AND m.session_id != ?2
              ORDER BY bm25(messages_fts)
              LIMIT ?3",
        )?;

        let rows = stmt
            .query_map(params![match_query, self.session_id, limit], |row| {
                Ok(MemoryHit {
                    session_id: row.get(0)?,
                    created_at: row.get(1)?,
                    role: row.get(2)?,
                    content: row.get(3)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(rows)
    }

    pub async fn close(&self) -> JumabekResult<()> {
        let conn = self.conn.lock().await;
        conn.execute(
            "UPDATE sessions SET ended_at = ?1 WHERE id = ?2",
            params![Utc::now().to_rfc3339(), self.session_id],
        )?;
        Ok(())
    }
}

fn migrate(conn: &Connection) -> JumabekResult<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;

    if version < schema::SCHEMA_VERSION {
        conn.execute_batch(schema::DROP_LEGACY_INDEX)?;
        rebuild_facts(conn)?;
        conn.execute_batch(schema::SCHEMA)?;
        add_missing_columns(conn)?;
        reindex(conn)?;
        let trimmed = trim_stored_json(conn)?;
        if trimmed > 0 {
            eprintln!(
                "[memory] cleaned {} stored answer(s) that carried prose around the JSON",
                trimmed
            );
        }
        conn.execute_batch(&format!("PRAGMA user_version = {}", schema::SCHEMA_VERSION))?;
    }

    Ok(())
}

fn rebuild_facts(conn: &Connection) -> JumabekResult<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(facts)")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;

    if existing.is_empty() || existing.iter().any(|name| name == "owner") {
        return Ok(());
    }

    let before: i64 = conn.query_row("SELECT count(*) FROM facts", [], |row| row.get(0))?;
    drop(stmt);

    conn.execute_batch(schema::REBUILD_FACTS)?;

    let after: i64 = conn.query_row("SELECT count(*) FROM facts", [], |row| row.get(0))?;
    if after < before {
        eprintln!(
            "[memory] warning: {} fact(s) went missing while widening the table",
            before - after
        );
    } else if before > 0 {
        eprintln!("[memory] {} fact(s) carried over to the wider table", after);
    }

    Ok(())
}

fn add_missing_columns(conn: &Connection) -> JumabekResult<()> {
    let mut stmt = conn.prepare("PRAGMA table_info(messages)")?;
    let existing: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;

    for column in ["level", "level_change"] {
        if !existing.iter().any(|name| name == column) {
            conn.execute_batch(&format!("ALTER TABLE messages ADD COLUMN {} TEXT", column))?;
        }
    }

    Ok(())
}

fn trim_stored_json(conn: &Connection) -> JumabekResult<usize> {
    let rows: Vec<(i64, String)> = {
        let mut stmt =
            conn.prepare("SELECT id, raw_json FROM messages WHERE raw_json IS NOT NULL")?;
        let found = stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        found.collect::<Result<Vec<_>, _>>()?
    };

    let mut changed = 0;

    for (id, stored) in rows {
        let payload = crate::core::json_repair::extract_json_payload(&stored);

        if payload == stored || serde_json::from_str::<serde_json::Value>(&payload).is_err() {
            continue;
        }

        conn.execute(
            "UPDATE messages SET raw_json = ?1 WHERE id = ?2",
            params![payload, id],
        )?;
        changed += 1;
    }

    Ok(changed)
}

fn reindex(conn: &Connection) -> JumabekResult<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, content FROM messages WHERE role IN ('user', 'assistant') ORDER BY id",
    )?;

    let rows: Vec<(i64, String)> = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;

    let mut indexed = 0;
    for (id, content) in rows {
        let searchable = query::to_search_text(&content);
        if searchable.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO messages_fts(rowid, content) VALUES (?1, ?2)",
            params![id, searchable],
        )?;
        indexed += 1;
    }

    Ok(indexed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seeded(rows: &[&str]) -> Connection {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(schema::SCHEMA).expect("schema");
        conn.execute(
            "INSERT INTO sessions (started_at, interface) VALUES ('now', 'cli')",
            [],
        )
        .expect("session");

        for raw in rows {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, raw_json, created_at)
                 VALUES (1, 'assistant', 'content', ?1, 'now')",
                params![raw],
            )
            .expect("row");
        }

        conn
    }

    fn stored(conn: &Connection) -> Vec<Option<String>> {
        let mut stmt = conn
            .prepare("SELECT raw_json FROM messages ORDER BY id")
            .unwrap();
        let rows = stmt.query_map([], |row| row.get(0)).unwrap();
        rows.collect::<Result<Vec<_>, _>>().unwrap()
    }

    const ANSWER: &str = r#"{"message":"ok","is_done":false,"actions":[]}"#;

    #[test]
    fn prose_in_front_of_a_stored_answer_is_cut_away() {
        let conn = seeded(&[&format!("Отвечаю с новым шагом.\n\n{}", ANSWER)]);

        assert_eq!(trim_stored_json(&conn).unwrap(), 1);
        assert_eq!(stored(&conn)[0].as_deref(), Some(ANSWER));
    }

    #[test]
    fn an_answer_that_was_already_clean_is_not_rewritten() {
        let conn = seeded(&[ANSWER]);

        assert_eq!(
            trim_stored_json(&conn).unwrap(),
            0,
            "a clean row was rewritten for no reason"
        );
        assert_eq!(stored(&conn)[0].as_deref(), Some(ANSWER));
    }

    #[test]
    fn a_row_with_no_json_in_it_is_left_alone() {
        let conn = seeded(&["the model wrote only prose this time"]);

        assert_eq!(trim_stored_json(&conn).unwrap(), 0);
        assert_eq!(
            stored(&conn)[0].as_deref(),
            Some("the model wrote only prose this time"),
            "a row that could not be improved was damaged instead"
        );
    }

    #[test]
    fn a_fence_and_a_thought_are_both_removed() {
        let conn = seeded(&[
            &format!("<think>planning</think>\n```json\n{}\n```", ANSWER),
            &format!("Sure! Here you go:\n{}\nHope that helps.", ANSWER),
        ]);

        assert_eq!(trim_stored_json(&conn).unwrap(), 2);
        for row in stored(&conn) {
            let text = row.expect("row kept");
            serde_json::from_str::<serde_json::Value>(&text).expect("not JSON");
            assert!(text.starts_with('{'), "got: {text}");
        }
    }

    #[test]
    fn cleaning_the_same_database_twice_changes_nothing_the_second_time() {
        let conn = seeded(&[&format!("preamble\n{}", ANSWER)]);

        assert_eq!(trim_stored_json(&conn).unwrap(), 1);
        assert_eq!(trim_stored_json(&conn).unwrap(), 0);
    }

    #[test]
    fn a_database_without_the_level_column_gets_one() {
        let conn = Connection::open_in_memory().expect("in-memory db");
        conn.execute_batch(
            "CREATE TABLE messages (
                 id             INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id     INTEGER NOT NULL,
                 task_id        TEXT,
                 parent_task_id TEXT,
                 role           TEXT NOT NULL,
                 content        TEXT NOT NULL,
                 raw_json       TEXT,
                 created_at     TEXT NOT NULL
             )",
        )
        .expect("old table");

        add_missing_columns(&conn).expect("migration");

        let mut stmt = conn.prepare("PRAGMA table_info(messages)").unwrap();
        let names: Vec<String> = stmt
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();

        assert!(names.iter().any(|n| n == "level"), "{names:?}");
        assert!(names.iter().any(|n| n == "level_change"), "{names:?}");
    }

    #[test]
    fn adding_the_column_twice_is_not_an_error() {
        let conn = seeded(&[]);
        add_missing_columns(&conn).expect("first");
        add_missing_columns(&conn).expect("second");
    }

    #[test]
    fn the_migration_runs_the_cleanup_on_an_old_database() {
        let conn = seeded(&[&format!("Отвечаю с новым шагом.\n\n{}", ANSWER)]);
        conn.execute_batch("PRAGMA user_version = 2").unwrap();

        migrate(&conn).unwrap();

        assert_eq!(stored(&conn)[0].as_deref(), Some(ANSWER));
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, schema::SCHEMA_VERSION);
    }
}
