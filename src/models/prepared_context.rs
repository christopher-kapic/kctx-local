use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use uuid::Uuid;

/// A prepared orientation map for a package (global or branch-scoped).
/// Stored by `kcl prepare` and injected into `ask` prompts when available.
#[derive(Debug, Clone)]
pub struct PreparedContext {
    pub id: String,
    pub package_id: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
    pub harness: String,
    pub model: Option<String>,
    pub git_commit_sha: Option<String>,
    pub git_branch: Option<String>,
    pub prepare_scope_at_time: String,
}

impl PreparedContext {
    /// Create a new PreparedContext (generates id + timestamp).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        package_id: String,
        content: String,
        harness: String,
        model: Option<String>,
        git_commit_sha: Option<String>,
        git_branch: Option<String>,
        prepare_scope_at_time: String,
    ) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            package_id,
            content,
            created_at: Utc::now(),
            harness,
            model,
            git_commit_sha,
            git_branch,
            prepare_scope_at_time,
        }
    }

    /// Insert (replacing any prior map for the same (package_id, git_branch) key,
    /// where NULL branch represents the `global` scope).
    /// The delete + insert is performed inside a transaction for safety.
    pub fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute("BEGIN IMMEDIATE", [])?;

        // Remove prior entry for this scope key so re-prepare replaces.
        if self.git_branch.is_none() {
            conn.execute(
                "DELETE FROM package_prepared_contexts WHERE package_id = ?1 AND git_branch IS NULL",
                params![self.package_id],
            )?;
        } else {
            conn.execute(
                "DELETE FROM package_prepared_contexts WHERE package_id = ?1 AND git_branch = ?2",
                params![self.package_id, &self.git_branch],
            )?;
        }

        let res = conn.execute(
            "INSERT INTO package_prepared_contexts (id, package_id, content, created_at, harness, model, git_commit_sha, git_branch, prepare_scope_at_time)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                self.id,
                self.package_id,
                self.content,
                self.created_at.to_rfc3339(),
                self.harness,
                self.model,
                self.git_commit_sha,
                self.git_branch,
                self.prepare_scope_at_time,
            ],
        );

        match res {
            Ok(_) => {
                conn.execute("COMMIT", [])?;
                Ok(())
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e).context("failed to insert prepared context")
            }
        }
    }

    /// Fetch the latest global (branch IS NULL) prepared context for the package.
    pub fn get_latest_for_package(
        conn: &Connection,
        package_id: &str,
    ) -> Result<Option<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, package_id, content, created_at, harness, model, git_commit_sha, git_branch, prepare_scope_at_time
             FROM package_prepared_contexts
             WHERE package_id = ?1 AND git_branch IS NULL
             ORDER BY created_at DESC LIMIT 1",
        )?;

        let mut rows = stmt.query(params![package_id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Fetch the latest prepared context for a specific branch.
    pub fn get_latest_for_package_and_branch(
        conn: &Connection,
        package_id: &str,
        branch: &str,
    ) -> Result<Option<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, package_id, content, created_at, harness, model, git_commit_sha, git_branch, prepare_scope_at_time
             FROM package_prepared_contexts
             WHERE package_id = ?1 AND git_branch = ?2
             ORDER BY created_at DESC LIMIT 1",
        )?;

        let mut rows = stmt.query(params![package_id, branch])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Convenience helper that ask (and future callers) can use to select the
    /// right getter based on the package's prepare_scope setting and the
    /// branch currently checked out. Matches the exact pattern described in
    /// the implementation plan.
    #[allow(dead_code)]
    pub fn get_latest_for_package_respecting_scope(
        conn: &Connection,
        package_id: &str,
        wants_per_branch: bool,
        current_branch: Option<&str>,
    ) -> Result<Option<Self>> {
        if wants_per_branch {
            if let Some(b) = current_branch {
                Self::get_latest_for_package_and_branch(conn, package_id, b)
            } else {
                // Local package or detached HEAD: fall back to any global map if present.
                Self::get_latest_for_package(conn, package_id)
            }
        } else {
            Self::get_latest_for_package(conn, package_id)
        }
    }

    fn from_row(row: &rusqlite::Row) -> Result<Self> {
        let created_str: String = row.get(3)?;
        Ok(Self {
            id: row.get(0)?,
            package_id: row.get(1)?,
            content: row.get(2)?,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .context("invalid created_at in prepared context")?
                .with_timezone(&Utc),
            harness: row.get(4)?,
            model: row.get(5)?,
            git_commit_sha: row.get(6)?,
            git_branch: row.get(7)?,
            prepare_scope_at_time: row.get(8)?,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::package::{Package, SourceType};

    fn insert_test_package(conn: &rusqlite::Connection, identifier: &str) -> Package {
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
    fn insert_and_get_latest_global() {
        let conn = db::open_memory().unwrap();
        let pkg = insert_test_package(&conn, "prep-pkg");

        let ctx = PreparedContext::new(
            pkg.id.clone(),
            "global map here".to_string(),
            "claude".to_string(),
            Some("sonnet".to_string()),
            None,
            None,
            "global".to_string(),
        );
        ctx.insert(&conn).unwrap();

        let fetched = PreparedContext::get_latest_for_package(&conn, &pkg.id)
            .unwrap()
            .expect("should find global prepared");
        assert_eq!(fetched.content, "global map here");
        assert_eq!(fetched.prepare_scope_at_time, "global");
        assert!(fetched.git_branch.is_none());
    }

    #[test]
    fn insert_and_get_latest_for_branch() {
        let conn = db::open_memory().unwrap();
        let pkg = insert_test_package(&conn, "branch-prep");

        let ctx = PreparedContext::new(
            pkg.id.clone(),
            "main branch map".to_string(),
            "claude".to_string(),
            None,
            Some("abc123".to_string()),
            Some("main".to_string()),
            "branch".to_string(),
        );
        ctx.insert(&conn).unwrap();

        let fetched = PreparedContext::get_latest_for_package_and_branch(&conn, &pkg.id, "main")
            .unwrap()
            .expect("should find branch prepared");
        assert_eq!(fetched.content, "main branch map");
        assert_eq!(fetched.git_branch.as_deref(), Some("main"));
    }

    #[test]
    fn prepare_replaces_prior_for_scope() {
        let conn = db::open_memory().unwrap();
        let pkg = insert_test_package(&conn, "replace-prep");

        let first = PreparedContext::new(
            pkg.id.clone(),
            "first".to_string(),
            "claude".to_string(),
            None,
            None,
            None,
            "global".to_string(),
        );
        first.insert(&conn).unwrap();

        let second = PreparedContext::new(
            pkg.id.clone(),
            "second".to_string(),
            "claude".to_string(),
            None,
            None,
            None,
            "global".to_string(),
        );
        second.insert(&conn).unwrap();

        let latest = PreparedContext::get_latest_for_package(&conn, &pkg.id).unwrap().unwrap();
        assert_eq!(latest.content, "second");
    }
}
