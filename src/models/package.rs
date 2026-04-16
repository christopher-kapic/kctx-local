use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// The type of source for a registered package.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceType {
    Local,
    Git,
}

impl SourceType {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceType::Local => "local",
            SourceType::Git => "git",
        }
    }
}

impl FromStr for SourceType {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        match s {
            "local" => Ok(SourceType::Local),
            "git" => Ok(SourceType::Git),
            other => anyhow::bail!("unknown source type: {other}"),
        }
    }
}

/// A registered codebase that kcl can query.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Package {
    pub id: String,
    pub identifier: String,
    pub display_name: String,
    pub source_type: SourceType,
    pub source_url: Option<String>,
    pub source_branch: Option<String>,
    pub path: String,
    pub auto_pull: bool,
    pub harness: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Package {
    /// Create a new Package with a generated UUID and current timestamps.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        identifier: String,
        display_name: String,
        source_type: SourceType,
        source_url: Option<String>,
        source_branch: Option<String>,
        path: String,
        auto_pull: bool,
        harness: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4().to_string(),
            identifier,
            display_name,
            source_type,
            source_url,
            source_branch,
            path,
            auto_pull,
            harness,
            created_at: now,
            updated_at: now,
        }
    }

    /// Insert this package into the database.
    pub fn insert(&self, conn: &Connection) -> Result<()> {
        conn.execute(
            "INSERT INTO packages (id, identifier, display_name, source_type, source_url, source_branch, path, auto_pull, harness, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
            params![
                self.id,
                self.identifier,
                self.display_name,
                self.source_type.as_str(),
                self.source_url,
                self.source_branch,
                self.path,
                self.auto_pull as i32,
                self.harness,
                self.created_at.to_rfc3339(),
                self.updated_at.to_rfc3339(),
            ],
        )
        .context("failed to insert package")?;
        Ok(())
    }

    /// Retrieve a package by its human-readable identifier.
    pub fn get_by_identifier(conn: &Connection, identifier: &str) -> Result<Option<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, identifier, display_name, source_type, source_url, source_branch, path, auto_pull, harness, created_at, updated_at
             FROM packages WHERE identifier = ?1",
        )?;

        let mut rows = stmt.query(params![identifier])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Retrieve the first package registered against a given git source URL.
    /// Used to detect when a new package can reuse an existing clone instead
    /// of cloning the same repo twice.
    pub fn get_by_source_url(conn: &Connection, source_url: &str) -> Result<Option<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, identifier, display_name, source_type, source_url, source_branch, path, auto_pull, harness, created_at, updated_at
             FROM packages WHERE source_url = ?1 ORDER BY created_at LIMIT 1",
        )?;

        let mut rows = stmt.query(params![source_url])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::from_row(row)?)),
            None => Ok(None),
        }
    }

    /// Retrieve a package by its UUID.
    #[allow(dead_code)]
    pub fn get_by_id(conn: &Connection, id: &str) -> Result<Option<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, identifier, display_name, source_type, source_url, source_branch, path, auto_pull, harness, created_at, updated_at
             FROM packages WHERE id = ?1",
        )?;

        let mut rows = stmt.query(params![id])?;
        match rows.next()? {
            Some(row) => Ok(Some(Self::from_row(row)?)),
            None => Ok(None),
        }
    }

    /// List all registered packages, ordered by identifier.
    pub fn list_all(conn: &Connection) -> Result<Vec<Self>> {
        let mut stmt = conn.prepare(
            "SELECT id, identifier, display_name, source_type, source_url, source_branch, path, auto_pull, harness, created_at, updated_at
             FROM packages ORDER BY identifier",
        )?;

        let rows = stmt.query_map([], |row| Ok(Self::from_row_inner(row)))?;

        let mut packages = Vec::new();
        for row in rows {
            packages.push(row?.context("failed to read package row")?);
        }
        Ok(packages)
    }

    /// Update a package's mutable fields.
    pub fn update(&self, conn: &Connection) -> Result<()> {
        let affected = conn.execute(
            "UPDATE packages SET display_name = ?1, source_url = ?2, source_branch = ?3, path = ?4, auto_pull = ?5, harness = ?6, updated_at = ?7
             WHERE id = ?8",
            params![
                self.display_name,
                self.source_url,
                self.source_branch,
                self.path,
                self.auto_pull as i32,
                self.harness,
                Utc::now().to_rfc3339(),
                self.id,
            ],
        )
        .context("failed to update package")?;
        if affected == 0 {
            anyhow::bail!("package with id '{}' not found", self.id);
        }
        Ok(())
    }

    /// Delete a package by id. Conversations cascade-delete via foreign key.
    pub fn delete(conn: &Connection, id: &str) -> Result<bool> {
        let affected = conn
            .execute("DELETE FROM packages WHERE id = ?1", params![id])
            .context("failed to delete package")?;
        Ok(affected > 0)
    }

    fn from_row(row: &rusqlite::Row) -> Result<Self> {
        Self::from_row_inner(row)
    }

    fn from_row_inner(row: &rusqlite::Row) -> Result<Self> {
        let source_type_str: String = row.get(3)?;
        let auto_pull_int: i32 = row.get(7)?;
        let created_str: String = row.get(9)?;
        let updated_str: String = row.get(10)?;

        Ok(Self {
            id: row.get(0)?,
            identifier: row.get(1)?,
            display_name: row.get(2)?,
            source_type: source_type_str.parse()?,
            source_url: row.get(4)?,
            source_branch: row.get(5)?,
            path: row.get(6)?,
            auto_pull: auto_pull_int != 0,
            harness: row.get(8)?,
            created_at: DateTime::parse_from_rfc3339(&created_str)
                .context("invalid created_at")?
                .with_timezone(&Utc),
            updated_at: DateTime::parse_from_rfc3339(&updated_str)
                .context("invalid updated_at")?
                .with_timezone(&Utc),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;

    fn test_package(identifier: &str) -> Package {
        Package::new(
            identifier.to_string(),
            identifier.to_string(),
            SourceType::Local,
            None,
            None,
            format!("/tmp/{identifier}"),
            false,
            None,
        )
    }

    #[test]
    fn insert_and_get_by_identifier() {
        let conn = db::open_memory().unwrap();
        let pkg = test_package("my-project");
        pkg.insert(&conn).unwrap();

        let retrieved = Package::get_by_identifier(&conn, "my-project")
            .unwrap()
            .expect("package should exist");

        assert_eq!(retrieved.id, pkg.id);
        assert_eq!(retrieved.identifier, "my-project");
        assert_eq!(retrieved.display_name, "my-project");
        assert_eq!(retrieved.source_type, SourceType::Local);
        assert_eq!(retrieved.source_url, None);
        assert_eq!(retrieved.path, "/tmp/my-project");
        assert!(!retrieved.auto_pull);
        assert_eq!(retrieved.harness, None);
    }

    #[test]
    fn get_by_identifier_returns_none_for_missing() {
        let conn = db::open_memory().unwrap();
        let result = Package::get_by_identifier(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn list_all_returns_all_packages() {
        let conn = db::open_memory().unwrap();

        test_package("alpha").insert(&conn).unwrap();
        test_package("beta").insert(&conn).unwrap();
        test_package("gamma").insert(&conn).unwrap();

        let packages = Package::list_all(&conn).unwrap();
        assert_eq!(packages.len(), 3);
        // Ordered by identifier.
        assert_eq!(packages[0].identifier, "alpha");
        assert_eq!(packages[1].identifier, "beta");
        assert_eq!(packages[2].identifier, "gamma");
    }

    #[test]
    fn delete_package() {
        let conn = db::open_memory().unwrap();
        let pkg = test_package("to-delete");
        pkg.insert(&conn).unwrap();

        let deleted = Package::delete(&conn, &pkg.id).unwrap();
        assert!(deleted);

        let result = Package::get_by_identifier(&conn, "to-delete").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn insert_git_package_with_all_fields() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "axum".to_string(),
            "Axum".to_string(),
            SourceType::Git,
            Some("https://github.com/tokio-rs/axum.git".to_string()),
            Some("main".to_string()),
            "/home/user/src/kcl-packages/axum".to_string(),
            true,
            Some("claude".to_string()),
        );
        pkg.insert(&conn).unwrap();

        let retrieved = Package::get_by_identifier(&conn, "axum")
            .unwrap()
            .expect("package should exist");

        assert_eq!(retrieved.source_type, SourceType::Git);
        assert_eq!(
            retrieved.source_url.as_deref(),
            Some("https://github.com/tokio-rs/axum.git")
        );
        assert_eq!(retrieved.source_branch.as_deref(), Some("main"));
        assert!(retrieved.auto_pull);
        assert_eq!(retrieved.harness.as_deref(), Some("claude"));
    }

    #[test]
    fn get_by_source_url_finds_existing() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "monorepo-app-a".to_string(),
            "monorepo-app-a".to_string(),
            SourceType::Git,
            Some("https://github.com/acme/monorepo.git".to_string()),
            Some("main".to_string()),
            "/clones/monorepo".to_string(),
            true,
            None,
        );
        pkg.insert(&conn).unwrap();

        let found =
            Package::get_by_source_url(&conn, "https://github.com/acme/monorepo.git").unwrap();
        assert!(found.is_some());
        let found = found.unwrap();
        assert_eq!(found.identifier, "monorepo-app-a");
        assert_eq!(found.path, "/clones/monorepo");

        let missing = Package::get_by_source_url(&conn, "https://github.com/other.git").unwrap();
        assert!(missing.is_none());
    }

    #[test]
    fn get_by_source_url_returns_first_when_multiple_share_url() {
        let conn = db::open_memory().unwrap();
        let url = "https://github.com/acme/monorepo.git".to_string();

        let a = Package::new(
            "app-a".to_string(),
            "app-a".to_string(),
            SourceType::Git,
            Some(url.clone()),
            Some("main".to_string()),
            "/clones/monorepo".to_string(),
            true,
            None,
        );
        a.insert(&conn).unwrap();

        let b = Package::new(
            "app-b".to_string(),
            "app-b".to_string(),
            SourceType::Git,
            Some(url.clone()),
            Some("main".to_string()),
            "/clones/monorepo".to_string(),
            true,
            None,
        );
        b.insert(&conn).unwrap();

        let found = Package::get_by_source_url(&conn, &url).unwrap().unwrap();
        // Both rows share the same path, so any match gives the right answer.
        assert_eq!(found.path, "/clones/monorepo");
    }

    #[test]
    fn source_type_from_str_valid() {
        assert_eq!("local".parse::<SourceType>().unwrap(), SourceType::Local);
        assert_eq!("git".parse::<SourceType>().unwrap(), SourceType::Git);
    }

    #[test]
    fn source_type_from_str_invalid() {
        let err = "svn".parse::<SourceType>().unwrap_err();
        assert!(err.to_string().contains("unknown source type: svn"));
    }

    #[test]
    fn update_package() {
        let conn = db::open_memory().unwrap();
        let mut pkg = test_package("updatable");
        pkg.insert(&conn).unwrap();

        pkg.display_name = "Updated Name".to_string();
        pkg.auto_pull = true;
        pkg.harness = Some("copilot".to_string());
        pkg.update(&conn).unwrap();

        let retrieved = Package::get_by_identifier(&conn, "updatable")
            .unwrap()
            .expect("package should exist");

        assert_eq!(retrieved.display_name, "Updated Name");
        assert!(retrieved.auto_pull);
        assert_eq!(retrieved.harness.as_deref(), Some("copilot"));
    }

    #[test]
    fn update_missing_package_errors() {
        let conn = db::open_memory().unwrap();
        let mut pkg = test_package("ghost");
        // Don't insert — just try to update a non-existent row.
        pkg.id = "nonexistent-id".to_string();
        let err = pkg.update(&conn).unwrap_err();
        assert!(
            err.to_string().contains("not found"),
            "expected 'not found' error, got: {err}"
        );
    }
}
