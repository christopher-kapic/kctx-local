//! SQLite read/write helpers for the outline index.

use anyhow::Result;
use rusqlite::{Connection, Transaction, params};

use super::parser::{Callsite, Identifier, Import, ParsedFile, Symbol};

/// A symbol row joined from the index for `explore symbol`. `path` is relative
/// to the package root.
#[derive(Debug, Clone, serde::Serialize)]
pub struct SymbolRow {
    pub path: String,
    pub line: u32,
    pub end_line: Option<u32>,
    pub name: String,
    pub kind: String,
    pub parent: Option<String>,
    pub visibility: Option<String>,
    pub signature: Option<String>,
}

/// An identifier hit returned by `explore word`. `path` is relative to root.
#[derive(Debug, Clone, serde::Serialize)]
pub struct IdentifierHit {
    pub path: String,
    pub line: u32,
}

/// Delete all rows for `(root, file)`. The FK cascade clears the dependents
/// once the parent row in `outline_files` is removed.
pub fn delete_file(tx: &Transaction<'_>, root: &str, file: &str) -> Result<()> {
    tx.execute(
        "DELETE FROM outline_files WHERE root_path = ?1 AND file_path = ?2",
        params![root, file],
    )?;
    Ok(())
}

/// Replace every row for `(root, file)` with the freshly-parsed contents.
#[allow(clippy::too_many_arguments)]
pub fn replace_file(
    tx: &Transaction<'_>,
    package_id: &str,
    root: &str,
    file: &str,
    language: &str,
    mtime_ns: i64,
    size_bytes: i64,
    content_hash: &str,
    indexed_at: &str,
    parsed: &ParsedFile,
) -> Result<()> {
    // Clear old rows. FK cascade on outline_symbols / outline_imports /
    // outline_identifiers handles their cleanup.
    tx.execute(
        "DELETE FROM outline_files WHERE root_path = ?1 AND file_path = ?2",
        params![root, file],
    )?;

    tx.execute(
        "INSERT INTO outline_files
            (package_id, root_path, file_path, language, mtime_ns, size_bytes, content_hash, indexed_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![package_id, root, file, language, mtime_ns, size_bytes, content_hash, indexed_at],
    )?;

    for sym in &parsed.symbols {
        insert_symbol(tx, root, file, sym)?;
    }
    for imp in &parsed.imports {
        insert_import(tx, root, file, imp)?;
    }
    for ident in &parsed.identifiers {
        insert_identifier(tx, root, file, ident)?;
    }
    for cs in &parsed.callsites {
        insert_callsite(tx, root, file, cs)?;
    }

    Ok(())
}

/// Insert a resolved dep edge. `importee_file` is `None` when the raw target
/// couldn't be resolved (external crate, stdlib, broken path).
pub fn insert_dep(
    tx: &Transaction<'_>,
    root: &str,
    importer_file: &str,
    importee_file: Option<&str>,
    raw_target: &str,
    line: u32,
) -> Result<()> {
    tx.execute(
        "INSERT INTO outline_deps
            (root_path, importer_file, importee_file, raw_target, line)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![root, importer_file, importee_file, raw_target, line as i64],
    )?;
    Ok(())
}

fn insert_callsite(tx: &Transaction<'_>, root: &str, file: &str, cs: &Callsite) -> Result<()> {
    tx.execute(
        "INSERT INTO outline_callsites
            (root_path, caller_file, caller_line, caller_symbol, callee_name, callee_kind)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            root,
            file,
            cs.caller_line as i64,
            cs.caller_symbol,
            cs.callee_name,
            cs.callee_kind,
        ],
    )?;
    Ok(())
}

/// One edge from `outline_deps`. `importee` is None for unresolved imports.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DepEdge {
    pub importer: String,
    pub importee: Option<String>,
    pub raw_target: String,
    pub line: u32,
}

/// All resolved + unresolved dep edges originating from `importer`.
pub fn forward_edges(conn: &Connection, root: &str, importer: &str) -> Result<Vec<DepEdge>> {
    let mut stmt = conn.prepare(
        "SELECT importer_file, importee_file, raw_target, line
             FROM outline_deps
             WHERE root_path = ?1 AND importer_file = ?2
             ORDER BY line ASC",
    )?;
    let rows = stmt.query_map(params![root, importer], |r| {
        Ok(DepEdge {
            importer: r.get(0)?,
            importee: r.get::<_, Option<String>>(1)?,
            raw_target: r.get(2)?,
            line: r.get::<_, i64>(3)? as u32,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// All importers that resolved their import to `importee`.
pub fn reverse_importers(conn: &Connection, root: &str, importee: &str) -> Result<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT importer_file FROM outline_deps
             WHERE root_path = ?1 AND importee_file = ?2
             ORDER BY importer_file ASC",
    )?;
    let rows = stmt.query_map(params![root, importee], |r| r.get::<_, String>(0))?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Every resolved dep edge under `root`. Used by `circular` for SCC.
pub fn all_resolved_edges(conn: &Connection, root: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT importer_file, importee_file FROM outline_deps
             WHERE root_path = ?1 AND importee_file IS NOT NULL",
    )?;
    let rows = stmt.query_map(params![root], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// One callsite row joined for `impact`.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CallsiteRow {
    pub caller_file: String,
    pub caller_line: u32,
    pub caller_symbol: Option<String>,
    pub callee_name: String,
    pub callee_kind: String,
}

/// All callsites whose `callee_name` matches `name` exactly.
pub fn query_callsites(conn: &Connection, root: &str, name: &str) -> Result<Vec<CallsiteRow>> {
    let mut stmt = conn.prepare(
        "SELECT caller_file, caller_line, caller_symbol, callee_name, callee_kind
             FROM outline_callsites
             WHERE root_path = ?1 AND callee_name = ?2
             ORDER BY caller_file ASC, caller_line ASC",
    )?;
    let rows = stmt.query_map(params![root, name], |r| {
        Ok(CallsiteRow {
            caller_file: r.get(0)?,
            caller_line: r.get::<_, i64>(1)? as u32,
            caller_symbol: r.get(2)?,
            callee_name: r.get(3)?,
            callee_kind: r.get(4)?,
        })
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

fn insert_symbol(tx: &Transaction<'_>, root: &str, file: &str, s: &Symbol) -> Result<()> {
    tx.execute(
        "INSERT INTO outline_symbols
            (root_path, file_path, name, kind, line, end_line, parent, visibility, signature)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
        params![
            root,
            file,
            s.name,
            s.kind,
            s.line as i64,
            s.end_line.map(|n| n as i64),
            s.parent,
            s.visibility,
            s.signature,
        ],
    )?;
    Ok(())
}

fn insert_import(tx: &Transaction<'_>, root: &str, file: &str, i: &Import) -> Result<()> {
    tx.execute(
        "INSERT INTO outline_imports (root_path, file_path, target, line)
         VALUES (?1, ?2, ?3, ?4)",
        params![root, file, i.target, i.line as i64],
    )?;
    Ok(())
}

fn insert_identifier(tx: &Transaction<'_>, root: &str, file: &str, id: &Identifier) -> Result<()> {
    tx.execute(
        "INSERT INTO outline_identifiers (root_path, file_path, token, line)
         VALUES (?1, ?2, ?3, ?4)",
        params![root, file, id.token, id.line as i64],
    )?;
    Ok(())
}

/// Symbol record for one file (used by `explore outline`).
#[derive(Debug, Clone, serde::Serialize)]
pub struct OutlineRecord {
    pub language: String,
    pub content_hash: String,
    pub symbols: Vec<SymbolRow>,
    pub imports: Vec<ImportRow>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ImportRow {
    pub target: String,
    pub line: u32,
}

/// Read everything the index has for `(root, file)`. Returns `None` when the
/// file is not in the index.
pub fn get_outline(
    conn: &rusqlite::Connection,
    root: &str,
    file: &str,
) -> Result<Option<OutlineRecord>> {
    let meta: Option<(String, String)> = conn
        .query_row(
            "SELECT language, content_hash FROM outline_files
                 WHERE root_path = ?1 AND file_path = ?2",
            params![root, file],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((language, content_hash)) = meta else {
        return Ok(None);
    };

    let mut symbols: Vec<SymbolRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT name, kind, line, end_line, parent, visibility, signature
                 FROM outline_symbols
                 WHERE root_path = ?1 AND file_path = ?2
                 ORDER BY line ASC",
        )?;
        let rows = stmt.query_map(params![root, file], |r| {
            Ok(SymbolRow {
                path: file.to_string(),
                name: r.get(0)?,
                kind: r.get(1)?,
                line: r.get::<_, i64>(2)? as u32,
                end_line: r.get::<_, Option<i64>>(3)?.map(|n| n as u32),
                parent: r.get(4)?,
                visibility: r.get(5)?,
                signature: r.get(6)?,
            })
        })?;
        for row in rows {
            symbols.push(row?);
        }
    }

    let mut imports: Vec<ImportRow> = Vec::new();
    {
        let mut stmt = conn.prepare(
            "SELECT target, line FROM outline_imports
                 WHERE root_path = ?1 AND file_path = ?2
                 ORDER BY line ASC",
        )?;
        let rows = stmt.query_map(params![root, file], |r| {
            Ok(ImportRow {
                target: r.get(0)?,
                line: r.get::<_, i64>(1)? as u32,
            })
        })?;
        for row in rows {
            imports.push(row?);
        }
    }

    Ok(Some(OutlineRecord {
        language,
        content_hash,
        symbols,
        imports,
    }))
}

/// Look up symbols by name (or prefix) within a target's `root_path`. When
/// `kind_filter` is `Some`, only that `kind` is returned.
pub fn query_symbols(
    conn: &rusqlite::Connection,
    root: &str,
    name: &str,
    prefix: bool,
    kind_filter: Option<&str>,
) -> Result<Vec<SymbolRow>> {
    let mut sql = String::from(
        "SELECT file_path, name, kind, line, end_line, parent, visibility, signature
             FROM outline_symbols
             WHERE root_path = ?1 AND ",
    );
    if prefix {
        sql.push_str("name LIKE ?2 ESCAPE '\\'");
    } else {
        sql.push_str("name = ?2");
    }
    if kind_filter.is_some() {
        sql.push_str(" AND kind = ?3");
    }
    sql.push_str(" ORDER BY file_path ASC, line ASC");

    let name_param = if prefix {
        let escaped = name
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        format!("{}%", escaped)
    } else {
        name.to_string()
    };

    let mut stmt = conn.prepare(&sql)?;
    let map = |r: &rusqlite::Row<'_>| -> rusqlite::Result<SymbolRow> {
        Ok(SymbolRow {
            path: r.get(0)?,
            name: r.get(1)?,
            kind: r.get(2)?,
            line: r.get::<_, i64>(3)? as u32,
            end_line: r.get::<_, Option<i64>>(4)?.map(|n| n as u32),
            parent: r.get(5)?,
            visibility: r.get(6)?,
            signature: r.get(7)?,
        })
    };

    let mut out: Vec<SymbolRow> = Vec::new();
    if let Some(kind) = kind_filter {
        let rows = stmt.query_map(params![root, name_param, kind], map)?;
        for row in rows {
            out.push(row?);
        }
    } else {
        let rows = stmt.query_map(params![root, name_param], map)?;
        for row in rows {
            out.push(row?);
        }
    }
    Ok(out)
}

/// Look up identifier occurrences for `explore word`.
pub fn query_identifier(
    conn: &rusqlite::Connection,
    root: &str,
    token: &str,
    ignore_case: bool,
) -> Result<Vec<IdentifierHit>> {
    let (sql, bind) = if ignore_case {
        (
            "SELECT file_path, line FROM outline_identifiers
                 WHERE root_path = ?1 AND lower(token) = lower(?2)
                 ORDER BY file_path ASC, line ASC",
            token.to_string(),
        )
    } else {
        (
            "SELECT file_path, line FROM outline_identifiers
                 WHERE root_path = ?1 AND token = ?2
                 ORDER BY file_path ASC, line ASC",
            token.to_string(),
        )
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt.query_map(params![root, bind], |r| {
        Ok(IdentifierHit {
            path: r.get(0)?,
            line: r.get::<_, i64>(1)? as u32,
        })
    })?;
    let mut out: Vec<IdentifierHit> = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Return per-file symbol counts for every file known to the index under `root`,
/// including files that parsed cleanly but contained zero symbols.
///
/// Used by `explore tree` to annotate `[N sym]`. The LEFT JOIN on
/// `outline_files` is what distinguishes "indexed and empty" (`[0 sym]`) from
/// "not indexed at all" (no annotation) — the latter never appears in this map.
pub fn symbol_counts_by_file(
    conn: &rusqlite::Connection,
    root: &str,
) -> Result<std::collections::HashMap<String, usize>> {
    let mut stmt = conn.prepare(
        "SELECT f.file_path, COUNT(s.name)
             FROM outline_files f
             LEFT JOIN outline_symbols s
               ON s.root_path = f.root_path AND s.file_path = f.file_path
             WHERE f.root_path = ?1
             GROUP BY f.file_path",
    )?;
    let rows = stmt.query_map(params![root], |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
    })?;
    let mut out = std::collections::HashMap::new();
    for row in rows {
        let (k, v) = row?;
        out.insert(k, v);
    }
    Ok(out)
}
