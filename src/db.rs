use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Open (or create) the SQLite database at the given path and run migrations.
pub fn open(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create data directory: {}", parent.display()))?;
    }

    let conn = Connection::open(path)
        .with_context(|| format!("could not open database: {}", path.display()))?;

    // Enable WAL mode for better concurrent read performance.
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // Enable foreign key enforcement (off by default in SQLite).
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;

    migrate(&conn)?;

    Ok(conn)
}

/// Open an in-memory database — useful for tests.
#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("could not open in-memory database")?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Run all schema migrations. Uses a simple user_version check.
fn migrate(conn: &Connection) -> Result<()> {
    let version: i32 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;

    if version < 1 {
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS packages (
                id              TEXT PRIMARY KEY,
                identifier      TEXT NOT NULL UNIQUE,
                display_name    TEXT NOT NULL,
                source_type     TEXT NOT NULL,
                source_url      TEXT,
                source_branch   TEXT DEFAULT 'main',
                path            TEXT NOT NULL,
                auto_pull       INTEGER NOT NULL DEFAULT 0,
                harness         TEXT,
                created_at      TEXT NOT NULL,
                updated_at      TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS conversations (
                id              TEXT PRIMARY KEY,
                package_id      TEXT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
                question        TEXT NOT NULL,
                harness         TEXT NOT NULL,
                exit_code       INTEGER,
                log_path        TEXT NOT NULL,
                created_at      TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_conversations_package_id
                ON conversations(package_id);
            CREATE INDEX IF NOT EXISTS idx_conversations_created_at
                ON conversations(created_at);

            PRAGMA user_version = 1;
            ",
        )?;
    }

    if version < 2 {
        conn.execute_batch(
            "
            CREATE INDEX IF NOT EXISTS idx_conversations_pkg_created
                ON conversations(package_id, created_at DESC);

            DROP INDEX IF EXISTS idx_conversations_package_id;

            PRAGMA user_version = 2;
            ",
        )?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_create_tables() {
        let conn = open_memory().unwrap();

        // Verify packages table exists by querying it.
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM packages", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Verify conversations table exists.
        let count: i32 = conn
            .query_row("SELECT COUNT(*) FROM conversations", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn migrations_are_idempotent() {
        let conn = open_memory().unwrap();
        // Running migrate again should not fail.
        migrate(&conn).unwrap();
    }

    #[test]
    fn busy_timeout_set() {
        let conn = open_memory().unwrap();
        let timeout: i32 = conn
            .pragma_query_value(None, "busy_timeout", |row| row.get(0))
            .unwrap();
        assert_eq!(timeout, 5000);
    }

    #[test]
    fn compound_index_on_conversations() {
        let conn = open_memory().unwrap();

        // The compound index should exist.
        let has_compound: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_conversations_pkg_created'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(has_compound, "compound index should exist");

        // The old single-column package_id index should be dropped.
        let has_old: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master
                 WHERE type = 'index' AND name = 'idx_conversations_package_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!has_old, "old single-column package_id index should be dropped");
    }

    #[test]
    fn foreign_keys_enabled() {
        let conn = open_memory().unwrap();
        let fk: i32 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }
}
