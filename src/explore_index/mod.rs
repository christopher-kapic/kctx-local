//! Tree-sitter–backed persistent outline index.
//!
//! Indexes per-file symbols, imports, and identifiers into the existing SQLite
//! database (tables defined by migration 5). The index supports being built
//! lazily on-demand (via [`ensure_indexed`]) or eagerly (via [`index_target`],
//! used by `kcl prepare`). All commands that need symbol data go through this
//! module rather than re-parsing files inline.
//!
//! Schema overview (see `src/db.rs` migration 5):
//! - `outline_files`     — per-file content hash + mtime + language
//! - `outline_symbols`   — declared functions/types/etc. with line + visibility
//! - `outline_imports`   — `use` / `import` / `#include` strings per file
//! - `outline_identifiers` — every identifier-like token, for `explore word`

pub mod parser;
pub mod resolver;
pub mod store;

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use chrono::Utc;
use ignore::WalkBuilder;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

pub use parser::Language;

/// Files this large or larger are skipped (binary blobs, generated data, etc.).
const MAX_INDEXABLE_BYTES: u64 = 5 * 1024 * 1024;

/// Discover indexable files under `root` (gitignore-aware). Returns
/// (relative path, detected language). Files with an `Unknown` language
/// are skipped — they remain visible to other `kcl explore` commands
/// (`tree`, `read`, `search`, `hot`) but aren't index candidates.
pub fn discover_indexable_files(root: &Path) -> Result<Vec<(PathBuf, Language)>> {
    let mut out: Vec<(PathBuf, Language)> = Vec::new();
    let mut builder = WalkBuilder::new(root);
    builder
        .standard_filters(true)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .follow_links(false);

    for result in builder.build() {
        let Ok(dent) = result else { continue };
        if dent.depth() == 0 {
            continue;
        }
        let p = dent.path();
        if !dent.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let Some(lang) = Language::from_path(p) else {
            continue;
        };
        let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(u64::MAX);
        if size >= MAX_INDEXABLE_BYTES {
            continue;
        }
        let rel = match p.strip_prefix(root) {
            Ok(r) => r.to_path_buf(),
            Err(_) => continue,
        };
        out.push((rel, lang));
    }
    Ok(out)
}

/// Why a particular file is in `to_index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)]
pub enum StaleReason {
    Missing,
    MtimeChanged,
    HashChanged,
}

/// Per-target indexing plan: what to (re)index, what's already current,
/// and what should be evicted because the on-disk file is gone.
#[derive(Debug, Default)]
pub struct Plan {
    pub to_index: Vec<(PathBuf, Language, StaleReason)>,
    pub up_to_date: usize,
    pub removed: Vec<PathBuf>,
}

/// Counts emitted by [`index_target`]. Useful for the prepare-time summary line.
#[derive(Debug, Default)]
pub struct IndexStats {
    pub files_indexed: usize,
    pub files_failed: usize,
    pub symbols: usize,
    pub imports: usize,
    pub identifiers: usize,
}

/// Compute which files in `root` need to be (re)indexed.
pub fn compute_plan(conn: &Connection, root: &Path) -> Result<Plan> {
    let on_disk = discover_indexable_files(root)?;
    let root_str = root.to_string_lossy().into_owned();

    // Existing index rows for this root: file_path -> (mtime_ns, size_bytes, hash).
    let mut existing: std::collections::HashMap<String, (i64, i64, String)> =
        std::collections::HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT file_path, mtime_ns, size_bytes, content_hash
                 FROM outline_files WHERE root_path = ?1",
        )?;
        let rows = stmt.query_map([&root_str], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?;
        for row in rows {
            let (fp, mtime, size, hash) = row?;
            existing.insert(fp, (mtime, size, hash));
        }
    }

    let mut plan = Plan::default();
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (rel, lang) in on_disk {
        let rel_str = rel.to_string_lossy().into_owned();
        seen.insert(rel_str.clone());

        let abs = root.join(&rel);
        let meta = match std::fs::metadata(&abs) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime_ns = file_mtime_ns(&meta);
        let size = meta.len() as i64;

        match existing.get(&rel_str) {
            None => plan.to_index.push((rel, lang, StaleReason::Missing)),
            Some((existing_mtime, existing_size, existing_hash)) => {
                if *existing_mtime == mtime_ns && *existing_size == size {
                    plan.up_to_date += 1;
                } else {
                    // Cheaper than always hashing: if mtime/size moved, hash and compare.
                    let on_disk_hash = match hash_file(&abs) {
                        Ok(h) => h,
                        Err(_) => {
                            plan.to_index.push((rel, lang, StaleReason::MtimeChanged));
                            continue;
                        }
                    };
                    if on_disk_hash == *existing_hash {
                        // Content unchanged; we could bump the mtime row but
                        // it's harmless to leave it slightly stale.
                        plan.up_to_date += 1;
                    } else {
                        plan.to_index.push((rel, lang, StaleReason::HashChanged));
                    }
                }
            }
        }
    }

    for existing_path in existing.keys() {
        if !seen.contains(existing_path) {
            plan.removed.push(PathBuf::from(existing_path));
        }
    }

    Ok(plan)
}

/// Parse + write rows for one file. Idempotent: replaces any existing rows
/// for (root, file). Returns counts so callers can aggregate.
pub fn index_file(
    conn: &mut Connection,
    package_id: Option<&str>,
    root: &Path,
    rel: &Path,
    language: Language,
) -> Result<(usize, usize, usize)> {
    let abs = root.join(rel);
    let bytes =
        std::fs::read(&abs).with_context(|| format!("reading `{}` for indexing", abs.display()))?;
    let source = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return Ok((0, 0, 0)), // binary file slipped through
    };
    let hash = sha256_hex(&bytes);
    let meta = std::fs::metadata(&abs)?;
    let mtime_ns = file_mtime_ns(&meta);
    let size = meta.len() as i64;

    let parsed = parser::parse_file(language, &source)?;
    let now = Utc::now().to_rfc3339();
    let root_str = root.to_string_lossy().into_owned();
    let file_str = rel.to_string_lossy().into_owned();
    let pkg = package_id.unwrap_or("");

    let tx = conn.transaction()?;
    store::replace_file(
        &tx,
        pkg,
        &root_str,
        &file_str,
        language.as_str(),
        mtime_ns,
        size,
        &hash,
        &now,
        &parsed,
    )?;
    // Resolve each raw import to a concrete file under `root` (when
    // possible) and persist as dep edges. Unresolved imports still get a
    // row with `importee_file = NULL` so the agent can see them.
    for imp in &parsed.imports {
        let resolved = resolver::resolve_import(root, rel, language, &imp.target);
        let importee_str = resolved.as_ref().map(|p| p.to_string_lossy().into_owned());
        store::insert_dep(
            &tx,
            &root_str,
            &file_str,
            importee_str.as_deref(),
            &imp.target,
            imp.line,
        )?;
    }
    tx.commit()?;

    Ok((
        parsed.symbols.len(),
        parsed.imports.len(),
        parsed.identifiers.len(),
    ))
}

/// Index every file the plan flags as stale. Failures are logged + skipped —
/// one bad parse must not abort the whole run.
pub fn index_target<F: FnMut(&Path)>(
    conn: &mut Connection,
    package_id: Option<&str>,
    root: &Path,
    mut progress: F,
) -> Result<IndexStats> {
    let plan = compute_plan(conn, root)?;
    let mut stats = IndexStats::default();
    let root_str = root.to_string_lossy().into_owned();

    // Evict files no longer on disk.
    if !plan.removed.is_empty() {
        let tx = conn.transaction()?;
        for rel in &plan.removed {
            store::delete_file(&tx, &root_str, &rel.to_string_lossy())?;
        }
        tx.commit()?;
    }

    for (rel, lang, _reason) in plan.to_index {
        progress(&rel);
        match index_file(conn, package_id, root, &rel, lang) {
            Ok((s, i, d)) => {
                stats.files_indexed += 1;
                stats.symbols += s;
                stats.imports += i;
                stats.identifiers += d;
            }
            Err(e) => {
                stats.files_failed += 1;
                eprintln!("warning: failed to index `{}`: {:#}", rel.display(), e);
            }
        }
    }

    Ok(stats)
}

/// Lazy-fetch wrapper used by commands. If the file is not in the index OR
/// its content hash differs from the on-disk hash, re-index it before
/// returning. Files with `Unknown` language are silently skipped (caller
/// must handle the empty case).
pub fn ensure_indexed(
    conn: &mut Connection,
    package_id: Option<&str>,
    root: &Path,
    rel: &Path,
) -> Result<Language> {
    let abs = root.join(rel);
    let Some(lang) = Language::from_path(&abs) else {
        return Ok(Language::Unknown);
    };
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(_) => return Ok(lang),
    };
    if meta.len() >= MAX_INDEXABLE_BYTES {
        return Ok(lang);
    }

    let root_str = root.to_string_lossy().into_owned();
    let file_str = rel.to_string_lossy().into_owned();
    let on_disk_mtime = file_mtime_ns(&meta);
    let on_disk_size = meta.len() as i64;

    let existing: Option<(i64, i64, String)> = conn
        .query_row(
            "SELECT mtime_ns, size_bytes, content_hash
                 FROM outline_files
                 WHERE root_path = ?1 AND file_path = ?2",
            rusqlite::params![root_str, file_str],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .ok();

    let needs_reindex = match &existing {
        None => true,
        Some((mtime, size, _)) if *mtime == on_disk_mtime && *size == on_disk_size => false,
        Some((_, _, hash)) => match hash_file(&abs) {
            Ok(h) => h != *hash,
            Err(_) => true,
        },
    };

    if needs_reindex {
        index_file(conn, package_id, root, rel, lang)?;
    }
    Ok(lang)
}

fn hash_file(path: &Path) -> Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(sha256_hex(&bytes))
}

fn sha256_hex(bytes: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(bytes);
    let digest = h.finalize();
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(digest.len() * 2);
    for b in digest.iter() {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

fn file_mtime_ns(meta: &std::fs::Metadata) -> i64 {
    use std::time::UNIX_EPOCH;
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use tempfile::tempdir;

    #[test]
    fn index_file_round_trip_rust() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let file = root.join("lib.rs");
        std::fs::write(
            &file,
            "pub fn hello() {}\npub struct Foo { x: i32 }\nuse std::path::Path;\n",
        )
        .unwrap();

        let mut conn = db::open_memory().unwrap();
        let (symbols, imports, idents) =
            index_file(&mut conn, None, root, Path::new("lib.rs"), Language::Rust).unwrap();
        assert!(
            symbols >= 2,
            "expected at least fn + struct, got {}",
            symbols
        );
        assert!(imports >= 1, "expected use import, got {}", imports);
        assert!(idents > 0);

        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outline_files WHERE file_path = 'lib.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn ensure_indexed_lazily_indexes_then_skips_when_fresh() {
        let dir = tempdir().unwrap();
        let root = dir.path();
        let rel = Path::new("a.rs");
        std::fs::write(root.join(rel), "fn first() {}").unwrap();

        let mut conn = db::open_memory().unwrap();
        ensure_indexed(&mut conn, None, root, rel).unwrap();
        let n1: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outline_symbols WHERE file_path = 'a.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n1, 1);

        // Second call without changing the file: no re-parse, still 1 symbol.
        ensure_indexed(&mut conn, None, root, rel).unwrap();
        let n2: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outline_symbols WHERE file_path = 'a.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n2, 1);

        // After editing, expect updated symbol set on next call.
        // Bump mtime by a millisecond so the cheap mtime check trips re-hash.
        std::thread::sleep(std::time::Duration::from_millis(10));
        std::fs::write(root.join(rel), "fn first() {}\nfn second() {}").unwrap();
        ensure_indexed(&mut conn, None, root, rel).unwrap();
        let n3: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM outline_symbols WHERE file_path = 'a.rs'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n3, 2);
    }

    /// Smoke-test the full data path used by the three commands: index a few
    /// fixture files, then run the same queries that `outline`, `symbol`, and
    /// `word` issue against the store.
    #[test]
    fn commands_query_paths_against_index() {
        use super::store;

        let dir = tempdir().unwrap();
        let root = dir.path();
        std::fs::write(
            root.join("a.rs"),
            "pub fn foo() {}\npub struct Bar;\nfn helper() {}\n",
        )
        .unwrap();
        std::fs::create_dir_all(root.join("sub")).unwrap();
        std::fs::write(
            root.join("sub/b.rs"),
            "pub fn foo_other() {}\nfn helper() {}\n",
        )
        .unwrap();

        let mut conn = db::open_memory().unwrap();
        index_target(&mut conn, None, root, |_| {}).unwrap();

        let root_str = root.to_string_lossy().into_owned();

        // outline:
        let outline = store::get_outline(&conn, &root_str, "a.rs")
            .unwrap()
            .unwrap();
        assert!(outline.symbols.iter().any(|s| s.name == "foo"));
        assert!(outline.symbols.iter().any(|s| s.name == "Bar"));

        // symbol (exact):
        let exact = store::query_symbols(&conn, &root_str, "helper", false, None).unwrap();
        assert_eq!(exact.len(), 2, "helper appears in both files");
        assert!(exact[0].path < exact[1].path, "sorted by path");

        // symbol (prefix + kind filter):
        let prefix = store::query_symbols(&conn, &root_str, "foo", true, Some("function")).unwrap();
        assert_eq!(prefix.len(), 2, "foo + foo_other");

        // word:
        let hits = store::query_identifier(&conn, &root_str, "helper", false).unwrap();
        assert_eq!(hits.len(), 2);

        // tree per-file symbol counts:
        let counts = store::symbol_counts_by_file(&conn, &root_str).unwrap();
        assert!(counts.get("a.rs").copied().unwrap_or(0) >= 3);
        assert!(counts.get("sub/b.rs").copied().unwrap_or(0) >= 2);
    }
}
