use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Reject conversation-id inputs that contain anything other than lowercase
/// hex digits or `-`. UUID v4s only ever use that character set, so the check
/// never blocks a legitimate id; rejecting `%`, `_`, `\`, and any other
/// character prevents a SQL `LIKE` wildcard (or escape) from slipping in via
/// `get_by_id_or_prefix` and matching unrelated rows.
fn validate_id_or_prefix(id_or_prefix: &str) -> Result<()> {
    if id_or_prefix.is_empty() {
        anyhow::bail!("Conversation id must not be empty.");
    }
    if !id_or_prefix
        .chars()
        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase() || c == '-')
    {
        anyhow::bail!(
            "Conversation id `{id_or_prefix}` is invalid — expected lowercase hex digits or `-` (matching a UUID prefix)."
        );
    }
    Ok(())
}

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
    pub git_commit_sha: Option<String>,
    pub git_branch: Option<String>,
}

impl Conversation {
    /// Create a new Conversation with a generated UUID and current timestamp.
    #[allow(dead_code)]
    pub fn new(
        package_id: String,
        question: String,
        harness: String,
        exit_code: Option<i32>,
        log_path: String,
        git_commit_sha: Option<String>,
        git_branch: Option<String>,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            package_id,
            question,
            harness,
            exit_code,
            log_path,
            created_at: Utc::now(),
            git_commit_sha,
            git_branch,
        }
    }

    /// Insert this conversation into the database.
    pub fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO conversations (id, package_id, question, harness, exit_code, log_path, git_commit_sha, git_branch, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                self.id,
                self.package_id,
                self.question,
                self.harness,
                self.exit_code,
                self.log_path,
                self.git_commit_sha.clone(),
                self.git_branch.clone(),
                self.created_at.to_rfc3339(),
            ],
        )
        .context("failed to insert conversation")?;
        Ok(())
    }

    /// List conversations for a given package, ordered by most recent first.
    #[cfg(test)]
    pub fn list_by_package(conn: &Connection, package_id: &str) -> Result<Vec<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, package_id, question, harness, exit_code, log_path, git_commit_sha, git_branch, created_at
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
                    "SELECT id, package_id, question, harness, exit_code, log_path, git_commit_sha, git_branch, created_at
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
                    "SELECT id, package_id, question, harness, exit_code, log_path, git_commit_sha, git_branch, created_at
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
            "SELECT id, package_id, question, harness, exit_code, log_path, git_commit_sha, git_branch, created_at
             FROM conversations WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::from_row_inner(row)?)),
            None => Ok(None),
        }
    }

    /// Retrieve by exact ID or a unique short prefix (e.g. first 8 chars of UUID).
    /// Errors with a clear message (using backticks) if the prefix matches >1 row.
    /// This powers `kcl remember <short-id>`.
    ///
    /// `id_or_prefix` is restricted to the conversation-id character set
    /// (lowercase hex digits + `-`) before the SQL `LIKE` is run. UUIDs only
    /// ever contain those characters, so a legitimate input is always
    /// accepted; this prevents a `%` / `_` / `\` slipping into the LIKE
    /// pattern and silently matching unrelated rows.
    pub fn get_by_id_or_prefix(conn: &Connection, id_or_prefix: &str) -> Result<Option<Self>> {
        validate_id_or_prefix(id_or_prefix)?;

        // Fast path: exact match (handles full UUIDs or any exact id)
        if let Some(c) = Self::get_by_id(conn, id_or_prefix)? {
            return Ok(Some(c));
        }

        // Prefix search — UUIDs are lowercase hex, so prefix match is reliable.
        let mut stmt = conn.prepare(
            "SELECT id, package_id, question, harness, exit_code, log_path, git_commit_sha, git_branch, created_at
             FROM conversations WHERE id LIKE ?1 || '%' ORDER BY id",
        )?;
        let mut rows = stmt.query(params![id_or_prefix])?;

        let mut matches = Vec::new();
        while let Some(row) = rows.next()? {
            matches.push(Self::from_row_inner(row)?);
        }

        match matches.len() {
            0 => Ok(None),
            1 => Ok(Some(matches.into_iter().next().unwrap())),
            n => anyhow::bail!(
                "Conversation id prefix `{}` is ambiguous (matches {} conversations). Supply a longer prefix or the full ID.",
                id_or_prefix,
                n
            ),
        }
    }

    fn from_row_inner(row: &rusqlite::Row) -> Result<Self> {
        let created_str: String = row.get(8)?;
        Ok(Self {
            id: row.get(0)?,
            package_id: row.get(1)?,
            question: row.get(2)?,
            harness: row.get(3)?,
            exit_code: row.get(4)?,
            log_path: row.get(5)?,
            git_commit_sha: row.get(6)?,
            git_branch: row.get(7)?,
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
            false,
            "global".to_string(),
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
            None,
            None,
        );
        conv1.insert(&conn).unwrap();

        let conv2 = Conversation::new(
            pkg.id.clone(),
            "What is middleware?".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/logs/conv2.json".to_string(),
            None,
            None,
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
            None,
            None,
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
            None,
            None,
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
            None,
            None,
        );

        // Should fail due to foreign key constraint.
        assert!(conv.insert(&conn).is_err());
    }

    #[test]
    fn validate_id_or_prefix_accepts_uuids_and_short_prefixes() {
        super::validate_id_or_prefix("a1b2c3d4").unwrap();
        super::validate_id_or_prefix("a1b2c3d4-5678-1234-9abc-def012345678").unwrap();
        // hex with hyphens
        super::validate_id_or_prefix("a-b").unwrap();
    }

    #[test]
    fn validate_id_or_prefix_rejects_like_wildcards_and_other_chars() {
        for bad in &[
            "",
            "abc%",
            "_abc",
            "abc\\def",
            "ABCDEF",
            "abcg",
            "hello world",
            "../etc",
        ] {
            assert!(
                super::validate_id_or_prefix(bad).is_err(),
                "expected `{bad}` to be rejected"
            );
        }
    }

    #[test]
    fn get_by_id_or_prefix_rejects_like_wildcard() {
        let conn = db::open_memory().unwrap();
        // Even with no rows present, the validation gate must fire before SQL.
        let err = Conversation::get_by_id_or_prefix(&conn, "abc%").unwrap_err();
        assert!(err.to_string().contains("invalid"));
    }

    #[test]
    fn get_by_id_or_prefix_exact_and_short_unique() {
        let conn = db::open_memory().unwrap();
        let pkg = insert_test_package(&conn, "prefix-pkg");

        let conv1 = Conversation::new(
            pkg.id.clone(),
            "q1".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/1.json".to_string(),
            None,
            None,
        );
        conv1.insert(&conn).unwrap();

        let conv2 = Conversation::new(
            pkg.id.clone(),
            "q2".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/2.json".to_string(),
            None,
            None,
        );
        conv2.insert(&conn).unwrap();

        // Exact full id works.
        let by_full = Conversation::get_by_id_or_prefix(&conn, &conv1.id)
            .unwrap()
            .unwrap();
        assert_eq!(by_full.id, conv1.id);

        // Unique short prefix works (first 8 chars of a v4 uuid are almost always unique in tiny test set).
        let short = &conv2.id[..8];
        let by_short = Conversation::get_by_id_or_prefix(&conn, short)
            .unwrap()
            .unwrap();
        assert_eq!(by_short.id, conv2.id);

        // Non-existent prefix -> None.
        assert!(
            Conversation::get_by_id_or_prefix(&conn, "deadbeef")
                .unwrap()
                .is_none()
        );

        // If we had ambiguity we would error, but with only two rows a colliding 1-char prefix is unlikely;
        // we just assert the happy paths here.
    }
}
