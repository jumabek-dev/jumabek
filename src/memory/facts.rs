use chrono::Utc;
use rusqlite::{Connection, params};

use crate::error::JumabekResult;

pub const SUBJECT_LIMIT: usize = 64;
pub const VALUE_LIMIT: usize = 400;
pub const RENDER_LIMIT: usize = 120;

pub const LOCAL: &str = "me";
pub const SHARED: &str = "shared";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    Global,
    Language,
    Project,
}

impl Scope {
    pub fn parse(raw: &str) -> Option<Scope> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "" | "global" | "always" => Some(Scope::Global),
            "language" | "lang" => Some(Scope::Language),
            "project" | "repo" => Some(Scope::Project),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Global => "global",
            Scope::Language => "language",
            Scope::Project => "project",
        }
    }

    pub fn weight(self) -> f32 {
        match self {
            Scope::Global => 1.15,
            Scope::Language => 1.05,
            Scope::Project => 1.0,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Fact {
    pub owner: String,
    pub scope: Scope,
    pub scope_ref: String,
    pub pinned: bool,
    pub subject: String,
    pub key: String,
    pub value: String,
}

impl Fact {
    pub fn new(subject: &str, key: &str, value: &str) -> Fact {
        Fact {
            owner: LOCAL.to_string(),
            scope: Scope::Global,
            scope_ref: String::new(),
            pinned: false,
            subject: subject.to_string(),
            key: key.to_string(),
            value: value.to_string(),
        }
    }

    pub fn owned_by(mut self, owner: &str) -> Fact {
        self.owner = owner.to_string();
        self
    }

    pub fn about(mut self, scope: Scope, scope_ref: &str) -> Fact {
        self.scope = scope;
        self.scope_ref = normalise(scope_ref);
        self
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Written {
    pub kept: bool,
    pub replaced: Vec<String>,
}

pub fn owner_of(raw: &str) -> Option<String> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "" | LOCAL | "user" | "personal" | "mine" => Some(LOCAL.to_string()),
        SHARED | "everyone" | "common" => Some(SHARED.to_string()),
        _ => None,
    }
}

pub fn remember(conn: &Connection, fact: &Fact, also: bool) -> JumabekResult<Written> {
    let now = Utc::now().to_rfc3339();
    let owner = normalise(&fact.owner);
    let subject = normalise(&fact.subject);
    let key = normalise(&fact.key);
    let value = trim(&fact.value);

    let mut replaced = Vec::new();

    if !also {
        let mut stmt = conn.prepare(
            "SELECT value FROM facts
              WHERE owner = ?1 AND scope = ?2 AND scope_ref = ?3 AND subject = ?4 AND key = ?5
                AND value != ?6",
        )?;

        replaced = stmt
            .query_map(
                params![
                    owner,
                    fact.scope.as_str(),
                    fact.scope_ref,
                    subject,
                    key,
                    value
                ],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;

        if !replaced.is_empty() {
            conn.execute(
                "DELETE FROM facts
                  WHERE owner = ?1 AND scope = ?2 AND scope_ref = ?3 AND subject = ?4
                    AND key = ?5 AND value != ?6",
                params![
                    owner,
                    fact.scope.as_str(),
                    fact.scope_ref,
                    subject,
                    key,
                    value
                ],
            )?;
        }
    }

    let changed = conn.execute(
        "INSERT INTO facts (owner, scope, scope_ref, pinned, subject, key, value,
                            created_at, updated_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?8)
         ON CONFLICT(owner, scope, scope_ref, subject, key, value)
         DO UPDATE SET updated_at = ?8, pinned = ?4",
        params![
            owner,
            fact.scope.as_str(),
            fact.scope_ref,
            fact.pinned as i64,
            subject,
            key,
            value,
            now
        ],
    )?;

    Ok(Written {
        kept: changed > 0,
        replaced,
    })
}

pub fn forget(conn: &Connection, subject: &str, key: Option<&str>) -> JumabekResult<usize> {
    let subject = normalise(subject);

    let removed = match key {
        Some(key) => conn.execute(
            "DELETE FROM facts WHERE subject = ?1 AND key = ?2",
            params![subject, normalise(key)],
        )?,
        None => conn.execute("DELETE FROM facts WHERE subject = ?1", params![subject])?,
    };

    Ok(removed)
}

pub fn all(conn: &Connection) -> JumabekResult<Vec<Fact>> {
    visible_to(conn, LOCAL)
}

pub fn visible_to(conn: &Connection, owner: &str) -> JumabekResult<Vec<Fact>> {
    let mut stmt = conn.prepare(
        "SELECT owner, scope, scope_ref, pinned, subject, key, value
           FROM facts f
          WHERE (f.owner = ?1 OR f.owner = ?2)
            AND NOT (
                f.owner = ?2
                AND EXISTS (
                    SELECT 1 FROM facts mine
                     WHERE mine.owner = ?1
                       AND mine.scope = f.scope
                       AND mine.scope_ref = f.scope_ref
                       AND mine.subject = f.subject
                       AND mine.key = f.key
                )
            )
          ORDER BY subject = 'me' DESC, subject, key, id",
    )?;

    let rows = stmt
        .query_map(params![owner, SHARED], |row| {
            Ok(Fact {
                owner: row.get(0)?,
                scope: Scope::parse(&row.get::<_, String>(1)?).unwrap_or(Scope::Global),
                scope_ref: row.get(2)?,
                pinned: row.get::<_, i64>(3)? != 0,
                subject: row.get(4)?,
                key: row.get(5)?,
                value: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

pub fn awaiting_vectors(conn: &Connection, limit: usize) -> JumabekResult<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, subject, key, value FROM facts WHERE vector IS NULL ORDER BY id LIMIT ?1",
    )?;

    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                format!(
                    "{} {} {}",
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?
                ),
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

pub fn set_vector(conn: &Connection, id: i64, blob: &[u8]) -> JumabekResult<()> {
    conn.execute(
        "UPDATE facts SET vector = ?1 WHERE id = ?2",
        params![blob, id],
    )?;
    Ok(())
}

pub fn with_vectors(
    conn: &Connection,
    owner: &str,
) -> JumabekResult<Vec<crate::memory::retrieval::Candidate>> {
    let mut stmt = conn.prepare(
        "SELECT owner, scope, scope_ref, pinned, subject, key, value, vector
           FROM facts f
          WHERE (f.owner = ?1 OR f.owner = ?2)
            AND NOT (
                f.owner = ?2
                AND EXISTS (
                    SELECT 1 FROM facts mine
                     WHERE mine.owner = ?1
                       AND mine.scope = f.scope
                       AND mine.scope_ref = f.scope_ref
                       AND mine.subject = f.subject
                       AND mine.key = f.key
                )
            )
          ORDER BY subject = 'me' DESC, subject, key, id",
    )?;

    let rows = stmt
        .query_map(params![owner, SHARED], |row| {
            let blob: Option<Vec<u8>> = row.get(7)?;
            Ok(crate::memory::retrieval::Candidate {
                fact: Fact {
                    owner: row.get(0)?,
                    scope: Scope::parse(&row.get::<_, String>(1)?).unwrap_or(Scope::Global),
                    scope_ref: row.get(2)?,
                    pinned: row.get::<_, i64>(3)? != 0,
                    subject: row.get(4)?,
                    key: row.get(5)?,
                    value: row.get(6)?,
                },
                vector: blob.map(|b| crate::memory::retrieval::from_blob(&b)),
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows)
}

pub fn render(facts: &[Fact]) -> String {
    if facts.is_empty() {
        return String::new();
    }

    let mut lines: Vec<String> = Vec::new();
    let mut index = 0;

    while index < facts.len() && lines.len() < RENDER_LIMIT {
        let subject = facts[index].subject.clone();
        let mut parts: Vec<String> = Vec::new();

        while index < facts.len() && facts[index].subject == subject {
            let fact = &facts[index];
            let where_it_holds = match fact.scope {
                Scope::Global => String::new(),
                _ => format!(" ({} {})", fact.scope.as_str(), fact.scope_ref),
            };
            parts.push(format!("{}{}: {}", fact.key, where_it_holds, fact.value));
            index += 1;
        }

        lines.push(format!("{} — {}", subject, parts.join("; ")));
    }

    if index < facts.len() {
        lines.push(format!(
            "[{} more subjects are stored but not shown here]",
            facts[index..]
                .iter()
                .map(|f| f.subject.as_str())
                .collect::<std::collections::BTreeSet<_>>()
                .len()
        ));
    }

    lines.join("\n")
}

fn normalise(text: &str) -> String {
    let cleaned: String = text
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(SUBJECT_LIMIT)
        .collect();
    cleaned.trim().to_lowercase()
}

fn trim(text: &str) -> String {
    text.trim().chars().take(VALUE_LIMIT).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(crate::memory::schema::SCHEMA).unwrap();
        conn
    }

    fn fact(subject: &str, key: &str, value: &str) -> Fact {
        Fact::new(subject, key, value)
    }

    #[test]
    fn a_fact_survives_being_written_twice() {
        let conn = db();
        remember(&conn, &fact("Олжас", "telegram", "@olzhas"), false).unwrap();
        remember(&conn, &fact("олжас", "TELEGRAM", "@olzhas"), false).unwrap();

        let stored = all(&conn).unwrap();
        assert_eq!(
            stored.len(),
            1,
            "the same fact was stored twice: {stored:?}"
        );
    }

    #[test]
    fn writing_a_key_again_replaces_what_was_there() {
        let conn = db();
        remember(&conn, &fact("карго", "стек", "PostgreSQL"), false).unwrap();
        let written = remember(&conn, &fact("карго", "стек", "SQLite"), false).unwrap();

        let stored = all(&conn).unwrap();
        assert_eq!(stored.len(), 1, "the stale value stayed behind: {stored:?}");
        assert_eq!(stored[0].value, "SQLite");
        assert_eq!(
            written.replaced,
            vec!["PostgreSQL".to_string()],
            "the model was not told what it overwrote"
        );
    }

    #[test]
    fn a_key_can_hold_two_values_when_the_second_says_it_is_an_addition() {
        let conn = db();
        remember(&conn, &fact("Олжас", "phone", "+7771"), false).unwrap();
        let written = remember(&conn, &fact("Олжас", "phone", "+7772"), true).unwrap();

        let stored = all(&conn).unwrap();
        assert_eq!(stored.len(), 2, "the first phone was lost: {stored:?}");
        assert!(written.replaced.is_empty());
    }

    #[test]
    fn writing_the_very_same_value_again_replaces_nothing() {
        let conn = db();
        remember(&conn, &fact("me", "city", "Астана"), false).unwrap();
        let written = remember(&conn, &fact("me", "city", "Астана"), false).unwrap();

        assert!(written.replaced.is_empty(), "a no-op looked like a change");
        assert_eq!(all(&conn).unwrap().len(), 1);
    }

    #[test]
    fn several_keys_about_one_person_still_read_as_one_line() {
        let conn = db();
        remember(&conn, &fact("Олжас", "alias", "Балык"), false).unwrap();
        remember(&conn, &fact("Олжас", "telegram", "@olzhas"), false).unwrap();

        let rendered = render(&all(&conn).unwrap());
        assert!(rendered.contains("Балык"), "{rendered}");
        assert!(rendered.contains("@olzhas"), "{rendered}");
        assert_eq!(rendered.lines().count(), 1, "{rendered}");
    }

    #[test]
    fn a_personal_fact_hides_the_shared_one_it_disagrees_with() {
        let conn = db();
        remember(
            &conn,
            &fact("stack", "database", "PostgreSQL").owned_by(SHARED),
            false,
        )
        .unwrap();
        remember(
            &conn,
            &fact("stack", "queue", "RabbitMQ").owned_by(SHARED),
            false,
        )
        .unwrap();
        remember(&conn, &fact("stack", "database", "SQLite"), false).unwrap();

        let seen = visible_to(&conn, LOCAL).unwrap();
        let database: Vec<&str> = seen
            .iter()
            .filter(|f| f.key == "database")
            .map(|f| f.value.as_str())
            .collect();

        assert_eq!(database, vec!["SQLite"], "the shared fact was not shadowed");
        assert!(
            seen.iter().any(|f| f.key == "queue"),
            "shadowing one key hid another"
        );
    }

    #[test]
    fn shadowing_leaves_the_shared_fact_intact_for_everybody_else() {
        let conn = db();
        remember(
            &conn,
            &fact("stack", "database", "PostgreSQL").owned_by(SHARED),
            false,
        )
        .unwrap();
        remember(&conn, &fact("stack", "database", "SQLite"), false).unwrap();

        let theirs = visible_to(&conn, "someone-else").unwrap();
        assert_eq!(
            theirs.iter().map(|f| f.value.as_str()).collect::<Vec<_>>(),
            vec!["PostgreSQL"],
            "one person's opinion overwrote everyone's"
        );
    }

    #[test]
    fn the_same_key_in_two_scopes_is_two_facts() {
        let conn = db();
        remember(
            &conn,
            &fact("style", "tests", "table driven").about(Scope::Language, "rust"),
            false,
        )
        .unwrap();
        remember(
            &conn,
            &fact("style", "tests", "one file per case").about(Scope::Project, "crm"),
            false,
        )
        .unwrap();

        let stored = all(&conn).unwrap();
        assert_eq!(stored.len(), 2, "a scope was flattened away: {stored:?}");

        let rendered = render(&stored);
        assert!(rendered.contains("language rust"), "{rendered}");
        assert!(rendered.contains("project crm"), "{rendered}");
    }

    #[test]
    fn a_fact_can_only_be_put_against_the_user_or_everyone() {
        assert_eq!(owner_of(""), Some(LOCAL.to_string()));
        assert_eq!(owner_of("me"), Some(LOCAL.to_string()));
        assert_eq!(owner_of("SHARED"), Some(SHARED.to_string()));
        assert_eq!(
            owner_of("Олжас"),
            None,
            "the model was able to put a fact against another person"
        );
    }

    #[test]
    fn an_invented_scope_is_refused_rather_than_guessed() {
        assert_eq!(Scope::parse(""), Some(Scope::Global));
        assert_eq!(Scope::parse("project"), Some(Scope::Project));
        assert_eq!(Scope::parse("machine"), None);
    }

    #[test]
    fn a_broadly_true_fact_outweighs_a_narrow_one() {
        assert!(Scope::Global.weight() > Scope::Language.weight());
        assert!(Scope::Language.weight() > Scope::Project.weight());
    }

    #[test]
    fn what_is_known_about_the_user_comes_first() {
        let conn = db();
        remember(&conn, &fact("Олжас", "alias", "Балык"), false).unwrap();
        remember(&conn, &fact("me", "city", "Алматы"), false).unwrap();

        let rendered = render(&all(&conn).unwrap());
        assert!(rendered.starts_with("me —"), "{rendered}");
    }

    #[test]
    fn forgetting_takes_one_key_or_the_whole_subject() {
        let conn = db();
        remember(&conn, &fact("олжас", "alias", "Балык"), false).unwrap();
        remember(&conn, &fact("олжас", "telegram", "@olzhas"), false).unwrap();

        assert_eq!(forget(&conn, "олжас", Some("alias")).unwrap(), 1);
        assert_eq!(all(&conn).unwrap().len(), 1);

        assert_eq!(forget(&conn, "олжас", None).unwrap(), 1);
        assert!(all(&conn).unwrap().is_empty());
    }

    #[test]
    fn forgetting_something_unknown_is_not_an_error() {
        let conn = db();
        assert_eq!(forget(&conn, "nobody", None).unwrap(), 0);
    }

    #[test]
    fn nothing_known_renders_to_nothing() {
        let conn = db();
        assert!(render(&all(&conn).unwrap()).is_empty());
    }

    #[test]
    fn an_overlong_value_is_cut_rather_than_stored_whole() {
        let conn = db();
        remember(&conn, &fact("me", "note", &"x".repeat(1000)), false).unwrap();

        let stored = all(&conn).unwrap();
        assert_eq!(stored[0].value.chars().count(), VALUE_LIMIT);
    }

    #[test]
    fn a_crowded_memory_stops_short_and_says_so() {
        let conn = db();
        for i in 0..(RENDER_LIMIT + 20) {
            remember(&conn, &fact(&format!("person{:03}", i), "note", "x"), false).unwrap();
        }

        let rendered = render(&all(&conn).unwrap());
        assert_eq!(rendered.lines().count(), RENDER_LIMIT + 1);
        assert!(rendered.ends_with("not shown here]"), "{rendered}");
    }
}
