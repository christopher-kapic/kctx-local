use std::path::Path;

use anyhow::{Context, Result};
use rusqlite::Connection;

/// Open (or create) the SQLite database at the given path and run migrations.
pub fn open(path: &Path) -> Result<Connection> {
    open_inner(path, true)
}

/// Open the SQLite database at `path` without running schema migrations.
///
/// Use this only on hot paths that have already opened the DB (and therefore
/// already migrated) earlier in the same `kcl` invocation, but need to drop
/// and reopen the connection to release WAL locks across a long-running
/// subprocess (the harness). Caller guarantees the schema is already current.
pub fn open_no_migrate(path: &Path) -> Result<Connection> {
    open_inner(path, false)
}

fn open_inner(path: &Path, run_migrations: bool) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("could not create data directory: {}", parent.display()))?;
    }

    let conn = Connection::open(path)
        .with_context(|| format!("could not open database: {}", path.display()))?;

    apply_pragmas(&conn)?;
    if run_migrations {
        migrate(&conn)?;
    }

    Ok(conn)
}

/// Open an in-memory database — useful for tests.
#[cfg(test)]
pub fn open_memory() -> Result<Connection> {
    let conn = Connection::open_in_memory().context("could not open in-memory database")?;
    apply_pragmas(&conn)?;
    migrate(&conn)?;
    Ok(conn)
}

/// Apply connection-wide pragmas. SQLite silently ignores `journal_mode=WAL`
/// for `:memory:` databases (it stays in "memory" mode), so this helper is
/// safe to call from both disk-backed and in-memory `open*` paths.
fn apply_pragmas(conn: &Connection) -> Result<()> {
    // Enable WAL mode for better concurrent read performance (no-op for :memory:).
    conn.pragma_update(None, "journal_mode", "WAL")?;
    // Enable foreign key enforcement (off by default in SQLite).
    conn.pragma_update(None, "foreign_keys", "ON")?;
    // Block up to 5s on SQLITE_BUSY instead of failing immediately.
    conn.pragma_update(None, "busy_timeout", 5000)?;
    Ok(())
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
                source_branch   TEXT DEFAULT NULL,
                path            TEXT NOT NULL,
                auto_pull       INTEGER NOT NULL DEFAULT 0,
                harness         TEXT,
                shallow         INTEGER NOT NULL DEFAULT 0,
                prepare_scope   TEXT NOT NULL DEFAULT 'global',
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
                git_commit_sha  TEXT,
                git_branch      TEXT,
                created_at      TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_conversations_package_id
                ON conversations(package_id);
            CREATE INDEX IF NOT EXISTS idx_conversations_created_at
                ON conversations(created_at);

            -- New tables are also created for fresh DBs (the <4 block will
            -- also run for version=0 snapshots and CREATE IF NOT EXISTS).
            CREATE TABLE IF NOT EXISTS package_prepared_contexts (
                id TEXT PRIMARY KEY,
                package_id TEXT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                harness TEXT NOT NULL,
                model TEXT,
                git_commit_sha TEXT,
                git_branch TEXT,
                prepare_scope_at_time TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_prepared_contexts_package
                ON package_prepared_contexts(package_id);
            CREATE INDEX IF NOT EXISTS idx_prepared_contexts_pkg_created
                ON package_prepared_contexts(package_id, created_at DESC);

            CREATE UNIQUE INDEX IF NOT EXISTS uq_prepared_pkg_global
                ON package_prepared_contexts(package_id) WHERE git_branch IS NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS uq_prepared_pkg_branch
                ON package_prepared_contexts(package_id, git_branch) WHERE git_branch IS NOT NULL;

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

    if version < 3 {
        // Add the `shallow` column for optional shallow clones.
        // Use a best-effort ALTER: ignore "duplicate column" so that a fresh
        // DB whose v1 CREATE TABLE already contains the column (new installs)
        // does not fail the migration. Old v2 DBs will get the column added.
        if let Err(e) = conn.execute(
            "ALTER TABLE packages ADD COLUMN shallow INTEGER NOT NULL DEFAULT 0",
            [],
        ) {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("duplicate column") && !msg.contains("already exists") {
                return Err(e.into());
            }
        }
        conn.execute("PRAGMA user_version = 3", [])?;
    }

    if version < 4 {
        // Add `prepare_scope` (for global vs per-branch prepared context) and
        // provenance columns (commit + branch) recorded on conversations.
        // Safe ALTERs + CREATE IF NOT for the prepared-contexts table.
        // Idempotent for DBs that already have the columns (e.g. from v1 CREATE).
        if let Err(e) = conn.execute(
            "ALTER TABLE packages ADD COLUMN prepare_scope TEXT NOT NULL DEFAULT 'global'",
            [],
        ) {
            let msg = e.to_string().to_lowercase();
            if !msg.contains("duplicate column") && !msg.contains("already exists") {
                return Err(e.into());
            }
        }

        for col_sql in [
            "ALTER TABLE conversations ADD COLUMN git_commit_sha TEXT",
            "ALTER TABLE conversations ADD COLUMN git_branch TEXT",
        ] {
            if let Err(e) = conn.execute(col_sql, []) {
                let msg = e.to_string().to_lowercase();
                if !msg.contains("duplicate column") && !msg.contains("already exists") {
                    return Err(e.into());
                }
            }
        }

        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS package_prepared_contexts (
                id TEXT PRIMARY KEY,
                package_id TEXT NOT NULL REFERENCES packages(id) ON DELETE CASCADE,
                content TEXT NOT NULL,
                created_at TEXT NOT NULL,
                harness TEXT NOT NULL,
                model TEXT,
                git_commit_sha TEXT,
                git_branch TEXT,
                prepare_scope_at_time TEXT NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_prepared_contexts_package
                ON package_prepared_contexts(package_id);
            CREATE INDEX IF NOT EXISTS idx_prepared_contexts_pkg_created
                ON package_prepared_contexts(package_id, created_at DESC);

            CREATE UNIQUE INDEX IF NOT EXISTS uq_prepared_pkg_global
                ON package_prepared_contexts(package_id) WHERE git_branch IS NULL;
            CREATE UNIQUE INDEX IF NOT EXISTS uq_prepared_pkg_branch
                ON package_prepared_contexts(package_id, git_branch) WHERE git_branch IS NOT NULL;

            PRAGMA user_version = 4;
            ",
        )?;
    }

    if version < 5 {
        // Tree-sitter–backed outline index. Keyed primarily by `root_path`
        // (canonical absolute path of the explore target) rather than
        // `package_id`, so loose targets (no registered package) and renamed
        // packages don't collide. `package_id` on `outline_files` is purely
        // informational — empty string when no registered package matches.
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS outline_files (
                package_id    TEXT NOT NULL,
                root_path     TEXT NOT NULL,
                file_path     TEXT NOT NULL,
                language      TEXT NOT NULL,
                mtime_ns      INTEGER NOT NULL,
                size_bytes    INTEGER NOT NULL,
                content_hash  TEXT NOT NULL,
                indexed_at    TEXT NOT NULL,
                PRIMARY KEY (root_path, file_path)
            );

            CREATE TABLE IF NOT EXISTS outline_symbols (
                root_path     TEXT NOT NULL,
                file_path     TEXT NOT NULL,
                name          TEXT NOT NULL,
                kind          TEXT NOT NULL,
                line          INTEGER NOT NULL,
                end_line      INTEGER,
                parent        TEXT,
                visibility    TEXT,
                signature     TEXT,
                FOREIGN KEY (root_path, file_path)
                    REFERENCES outline_files(root_path, file_path) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_symbols_name
                ON outline_symbols(name);
            CREATE INDEX IF NOT EXISTS idx_symbols_root_file
                ON outline_symbols(root_path, file_path);

            CREATE TABLE IF NOT EXISTS outline_imports (
                root_path     TEXT NOT NULL,
                file_path     TEXT NOT NULL,
                target        TEXT NOT NULL,
                line          INTEGER NOT NULL,
                FOREIGN KEY (root_path, file_path)
                    REFERENCES outline_files(root_path, file_path) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_imports_root_file
                ON outline_imports(root_path, file_path);

            CREATE TABLE IF NOT EXISTS outline_identifiers (
                root_path     TEXT NOT NULL,
                file_path     TEXT NOT NULL,
                token         TEXT NOT NULL,
                line          INTEGER NOT NULL,
                FOREIGN KEY (root_path, file_path)
                    REFERENCES outline_files(root_path, file_path) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_identifiers_token
                ON outline_identifiers(token);
            CREATE INDEX IF NOT EXISTS idx_identifiers_root_file
                ON outline_identifiers(root_path, file_path);

            PRAGMA user_version = 5;
            ",
        )?;
    }

    if version < 6 {
        // Resolved file-to-file dependency edges. Populated incrementally by
        // index_file after raw imports are written. A NULL importee_file means
        // the import couldn't be resolved (external crate, stdlib, broken
        // path) — we still record it so the agent can see unresolved deps
        // without re-parsing.
        //
        // Files already indexed under <=v5 did not populate outline_deps;
        // wipe outline_files for any pre-existing root so a fresh index
        // pass populates the new table. (FK cascade clears children.)
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS outline_deps (
                root_path       TEXT NOT NULL,
                importer_file   TEXT NOT NULL,
                importee_file   TEXT,
                raw_target      TEXT NOT NULL,
                line            INTEGER NOT NULL,
                FOREIGN KEY (root_path, importer_file)
                    REFERENCES outline_files(root_path, file_path) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_deps_importer
                ON outline_deps(root_path, importer_file);
            CREATE INDEX IF NOT EXISTS idx_deps_importee
                ON outline_deps(root_path, importee_file);

            DELETE FROM outline_files;

            PRAGMA user_version = 6;
            ",
        )?;
    }

    if version < 7 {
        // Callsite extraction: every call_expression / type_identifier /
        // macro_invocation captured per file, mapped to its enclosing
        // function/method when known. The `impact` command queries this
        // table for symbol blast-radius lookups. Noisy by design — same-name
        // collisions across files are surfaced as candidate sites for the
        // agent / user to disambiguate.
        conn.execute_batch(
            "
            CREATE TABLE IF NOT EXISTS outline_callsites (
                root_path       TEXT NOT NULL,
                caller_file     TEXT NOT NULL,
                caller_line     INTEGER NOT NULL,
                caller_symbol   TEXT,
                callee_name     TEXT NOT NULL,
                callee_kind     TEXT NOT NULL,
                FOREIGN KEY (root_path, caller_file)
                    REFERENCES outline_files(root_path, file_path) ON DELETE CASCADE
            );
            CREATE INDEX IF NOT EXISTS idx_callsites_callee
                ON outline_callsites(callee_name);
            CREATE INDEX IF NOT EXISTS idx_callsites_caller
                ON outline_callsites(root_path, caller_file);

            PRAGMA user_version = 7;
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
        assert!(
            !has_old,
            "old single-column package_id index should be dropped"
        );
    }

    #[test]
    fn foreign_keys_enabled() {
        let conn = open_memory().unwrap();
        let fk: i32 = conn
            .pragma_query_value(None, "foreign_keys", |row| row.get(0))
            .unwrap();
        assert_eq!(fk, 1);
    }

    #[test]
    fn user_version_is_7_after_migrations() {
        let conn = open_memory().unwrap();
        let v: i32 = conn
            .pragma_query_value(None, "user_version", |row| row.get(0))
            .unwrap();
        assert_eq!(
            v, 7,
            "expected user_version=7 after outline+deps+callsites migrations"
        );
    }

    #[test]
    fn outline_index_tables_exist_after_v5() {
        let conn = open_memory().unwrap();
        for table in [
            "outline_files",
            "outline_symbols",
            "outline_imports",
            "outline_identifiers",
        ] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "table `{}` must exist after v5 migration", table);
        }
    }

    #[test]
    fn deps_and_callsites_tables_exist_after_v7() {
        let conn = open_memory().unwrap();
        for table in ["outline_deps", "outline_callsites"] {
            let exists: bool = conn
                .query_row(
                    "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                    [table],
                    |row| row.get(0),
                )
                .unwrap();
            assert!(exists, "table `{}` must exist after v7 migration", table);
        }
    }

    #[test]
    fn new_tables_and_columns_exist_after_migrations() {
        let conn = open_memory().unwrap();

        let table = "package_prepared_contexts";
        let exists: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM sqlite_master WHERE type = 'table' AND name = ?1",
                [table],
                |row| row.get(0),
            )
            .unwrap();
        assert!(exists, "table `{}` must exist after v4 migration", table);

        // prepare_scope on packages
        let has_ps: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('packages') WHERE name = 'prepare_scope'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(has_ps, 1, "`prepare_scope` column must exist on packages");

        // provenance columns on conversations
        let has_git_cols: i32 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('conversations') WHERE name IN ('git_commit_sha', 'git_branch')",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            has_git_cols, 2,
            "git_commit_sha + git_branch columns must exist on conversations"
        );
    }

    #[test]
    fn package_prepare_scope_roundtrips_and_helper() {
        use crate::models::package::{Package, SourceType};

        let conn = open_memory().unwrap();
        let mut pkg = Package::new(
            "prep-scope-test".to_string(),
            "Prep Scope".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/prep-scope".to_string(),
            false,
            None,
            false,
            "branch".to_string(),
        );
        pkg.insert(&conn).unwrap();

        let loaded = Package::get_by_identifier(&conn, "prep-scope-test")
            .unwrap()
            .expect("package exists");
        assert_eq!(loaded.prepare_scope, "branch");
        assert!(loaded.wants_per_branch_prepare());

        // mutate via update
        pkg.prepare_scope = "global".to_string();
        pkg.update(&conn).unwrap();

        let reloaded = Package::get_by_identifier(&conn, "prep-scope-test")
            .unwrap()
            .expect("still exists");
        assert_eq!(reloaded.prepare_scope, "global");
        assert!(!reloaded.wants_per_branch_prepare());
    }

    #[test]
    fn prepared_context_insert_and_get_latest_apis() {
        use crate::models::package::{Package, SourceType};
        use crate::models::prepared_context::PreparedContext;

        let mut conn = open_memory().unwrap();
        let pkg = Package::new(
            "prep-ctx-pkg".to_string(),
            "PrepCtx".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/prep-ctx".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        pkg.insert(&conn).unwrap();

        let pc = PreparedContext::new(
            pkg.id.clone(),
            "compact orientation map\n- start: src/main.rs\n- build: cargo build".to_string(),
            "claude".to_string(),
            Some("claude-3-5-sonnet".to_string()),
            Some("deadbeef123".to_string()),
            None,
            "global".to_string(),
        );
        pc.insert(&mut conn).unwrap();

        let latest = PreparedContext::get_latest_for_package(&conn, &pkg.id)
            .unwrap()
            .expect("global prepared context should be retrievable");
        assert_eq!(latest.content, pc.content);
        assert_eq!(latest.harness, "claude");
        assert_eq!(latest.prepare_scope_at_time, "global");
        assert!(latest.git_branch.is_none());

        // branch scoped path also works
        let pc_branch = PreparedContext::new(
            pkg.id.clone(),
            "branch-specific".to_string(),
            "claude".to_string(),
            None,
            None,
            Some("feature-x".to_string()),
            "branch".to_string(),
        );
        pc_branch.insert(&mut conn).unwrap();

        let b = PreparedContext::get_latest_for_package_and_branch(&conn, &pkg.id, "feature-x")
            .unwrap()
            .expect("branch prepared should exist");
        assert_eq!(b.content, "branch-specific");
    }
}
