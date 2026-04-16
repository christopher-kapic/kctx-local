use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
#[cfg(test)]
use uuid::Uuid;

/// A conversation log entry — an indexed record of a Q&A session.
/// The full response text lives in a JSON log file on disk; this struct
/// tracks metadata for fast listing and filtering.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Conversation {
    pub id: String,
    pub package_id: String,
    pub question: String,
    pub harness: String,
    pub exit_code: Option<i32>,
    pub log_path: String,
    pub created_at: DateTime<Utc>,
}

impl Conversation {
    /// Create a new Conversation with a generated UUID and current timestamp.
    #[cfg(test)]
    pub fn new(
        package_id: String,
        question: String,
        harness: String,
        exit_code: Option<i32>,
        log_path: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            package_id,
            question,
            harness,
            exit_code,
            log_path,
            created_at: Utc::now(),
        }
    }

    /// Insert this conversation into the database.
    pub fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO conversations (id, package_id, question, harness, exit_code, log_path, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.id,
                self.package_id,
                self.question,
                self.harness,
                self.exit_code,
                self.log_path,
                self.created_at.to_rfc3339(),
            ],
        )
        .context("failed to insert conversation")?;
        Ok(())
    }

    /// List conversations for a given package, ordered by most recent first.
    #[allow(dead_code)]
    pub fn list_by_package(conn: &Connection, package_id: &str) -> Result<Vec<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, package_id, question, harness, exit_code, log_path, created_at
             FROM conversations WHERE package_id = ?1 ORDER BY created_at DESC",
        )?;

        let rows = stmt.query_map(params![package_id], |row| Ok(Self::from_row_inner(row)))?;

        let mut conversations = Vec::new();
        for row in rows {
            conversations.push(row?.context("failed to read conversation row")?);
        }
        Ok(conversations)
    }

    /// Return the question text of the N most recent conversations for a package.
    pub fn recent_questions(
        conn: &Connection,
        package_id: &str,
        limit: u32,
    ) -> Result<Vec<String>> {
        let mut stmt = conn.prepare(
            "SELECT question FROM conversations WHERE package_id = ?1
             ORDER BY created_at DESC LIMIT ?2",
        )?;

        let rows = stmt.query_map(params![package_id, limit], |row| row.get::<_, String>(0))?;

        let mut questions = Vec::new();
        for row in rows {
            questions.push(row?);
        }
        Ok(questions)
    }

    /// List conversations for a package with optional filters.
    pub fn list_filtered(
        conn: &Connection,
        package_id: &str,
        limit: u32,
        since_days: Option<u32>,
    ) -> Result<Vec<Self>> {
        let mut conversations = Vec::new();
        match since_days {
            Some(days) => {
                let cutoff = Utc::now() - chrono::Duration::days(days as i64);
                let cutoff_str = cutoff.to_rfc3339();
                let mut stmt = conn.prepare(
                    "SELECT id, package_id, question, harness, exit_code, log_path, created_at
                     FROM conversations WHERE package_id = ?1 AND created_at >= ?2
                     ORDER BY created_at DESC LIMIT ?3",
                )?;
                let rows = stmt.query_map(params![package_id, cutoff_str, limit], |row| {
                    Ok(Self::from_row_inner(row))
                })?;
                for row in rows {
                    conversations.push(row?.context("failed to read conversation row")?);
                }
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT id, package_id, question, harness, exit_code, log_path, created_at
                     FROM conversations WHERE package_id = ?1
                     ORDER BY created_at DESC LIMIT ?2",
                )?;
                let rows = stmt.query_map(params![package_id, limit], |row| {
                    Ok(Self::from_row_inner(row))
                })?;
                for row in rows {
                    conversations.push(row?.context("failed to read conversation row")?);
                }
            }
        }
        Ok(conversations)
    }

    /// Retrieve a single conversation by its UUID.
    pub fn get_by_id(conn: &Connection, id: &str) -> Result<Option<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, package_id, question, harness, exit_code, log_path, created_at
             FROM conversations WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::from_row_inner(row)?)),
            None => Ok(None),
        }
    }

    fn from_row_inner(row: &rusqlite::Row) -> Result<Self> {
        let created_str: String = row.get(6)?;
        Ok(Self {
            id: row.get(0)?,
            package_id: row.get(1)?,
            question: row.get(2)?,
            harness: row.get(3)?,
            exit_code: row.get(4)?,
            log_path: row.get(5)?,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .context("invalid created_at")?
                .with_timezone(&Utc),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::package::{Package, SourceType};

    fn insert_test_package(conn: &Connection, identifier: &str) -> Package {
        let pkg = Package::new(
            identifier.to_string(),
            identifier.to_string(),
            SourceType::Local,
            None,
            None,
            format!("/tmp/{identifier}"),
            false,
            None,
        );
        pkg.insert(conn).unwrap();
        pkg
    }

    #[test]
    fn insert_and_list_by_package() {
        let conn = db::open_memory().unwrap();
        let pkg = insert_test_package(&conn, "test-pkg");

        let conv1 = Conversation::new(
            pkg.id.clone(),
            "How does routing work?".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/logs/conv1.json".to_string(),
        );
        conv1.insert(&conn).unwrap();

        let conv2 = Conversation::new(
            pkg.id.clone(),
            "What is middleware?".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/logs/conv2.json".to_string(),
        );
        conv2.insert(&conn).unwrap();

        let conversations = Conversation::list_by_package(&conn, &pkg.id).unwrap();
        assert_eq!(conversations.len(), 2);
        // Most recent first.
        assert_eq!(conversations[0].question, "What is middleware?");
        assert_eq!(conversations[1].question, "How does routing work?");
    }

    #[test]
    fn get_by_id() {
        let conn = db::open_memory().unwrap();
        let pkg = insert_test_package(&conn, "test-pkg-2");

        let conv = Conversation::new(
            pkg.id.clone(),
            "What are extractors?".to_string(),
            "copilot".to_string(),
            None,
            "/tmp/logs/conv3.json".to_string(),
        );
        conv.insert(&conn).unwrap();

        let retrieved = Conversation::get_by_id(&conn, &conv.id)
            .unwrap()
            .expect("conversation should exist");

        assert_eq!(retrieved.id, conv.id);
        assert_eq!(retrieved.package_id, pkg.id);
        assert_eq!(retrieved.question, "What are extractors?");
        assert_eq!(retrieved.harness, "copilot");
        assert_eq!(retrieved.exit_code, None);
        assert_eq!(retrieved.log_path, "/tmp/logs/conv3.json");
    }

    #[test]
    fn get_by_id_returns_none_for_missing() {
        let conn = db::open_memory().unwrap();
        let result = Conversation::get_by_id(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn delete_package_cascades_to_conversations() {
        let conn = db::open_memory().unwrap();
        let pkg = insert_test_package(&conn, "cascade-pkg");

        let conv = Conversation::new(
            pkg.id.clone(),
            "Will this cascade?".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/logs/cascade.json".to_string(),
        );
        conv.insert(&conn).unwrap();

        // Verify conversation exists.
        assert!(Conversation::get_by_id(&conn, &conv.id).unwrap().is_some());

        // Delete the package.
        Package::delete(&conn, &pkg.id).unwrap();

        // Conversation should be gone.
        assert!(Conversation::get_by_id(&conn, &conv.id).unwrap().is_none());
    }

    #[test]
    fn foreign_key_prevents_orphan_conversations() {
        let conn = db::open_memory().unwrap();

        let conv = Conversation::new(
            "nonexistent-package-id".to_string(),
            "orphan question".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/logs/orphan.json".to_string(),
        );

        // Should fail due to foreign key constraint.
        assert!(conv.insert(&conn).is_err());
    }
}
