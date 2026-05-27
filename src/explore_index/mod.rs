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

use anyhow::Result;
use chrono::Utc;
use ignore::WalkBuilder;
use rayon::prelude::*;
use rusqlite::Connection;
use sha2::{Digest, Sha256};

pub use parser::Language;

/// Chunk size for the parallel parse → serial write pipeline. Bounds peak
/// memory to roughly N files' worth of `ParsedFile` data at a time, while
/// still giving rayon enough work per chunk to saturate cores on large repos.
const PARSE_CHUNK_SIZE: usize = 200;

/// Above this number of stale files in a single `bring_up_to_date` call we
/// emit a one-shot stderr line so agents and humans know the cold-cache pass
/// is doing real work (the symptom the Terraform-provider report identified —
/// `kcl explore symbol` appearing to hang on first call).
const COLD_PROGRESS_THRESHOLD: usize = 100;

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

/// Parsed file + the metadata needed to upsert it. Produced by the
/// parallel parse pass; consumed by the serial write pass in [`apply_plan`].
struct ParsedFileEntry {
    rel: PathBuf,
    language: Language,
    parsed: parser::ParsedFile,
    hash: String,
    mtime_ns: i64,
    size: i64,
}

/// Outcome of trying to parse one stale file. `Skip` means the file isn't
/// indexable (binary content, vanished from disk) — not an error.
enum ParseOutcome {
    Ok(ParsedFileEntry),
    Skip,
    Err(PathBuf, anyhow::Error),
}

/// Read + parse a single file. Pure CPU/IO with no DB access, so safe to
/// run from a rayon worker thread.
fn parse_one(root: &Path, rel: PathBuf, language: Language) -> ParseOutcome {
    let abs = root.join(&rel);
    let bytes = match std::fs::read(&abs) {
        Ok(b) => b,
        Err(e) => {
            return ParseOutcome::Err(
                rel,
                anyhow::Error::new(e).context(format!("reading `{}` for indexing", abs.display())),
            );
        }
    };
    let source = match std::str::from_utf8(&bytes) {
        Ok(s) => s.to_string(),
        Err(_) => return ParseOutcome::Skip,
    };
    let meta = match std::fs::metadata(&abs) {
        Ok(m) => m,
        Err(e) => return ParseOutcome::Err(rel, anyhow::Error::new(e)),
    };
    let parsed = match parser::parse_file(language, &source) {
        Ok(p) => p,
        Err(e) => return ParseOutcome::Err(rel, e),
    };
    ParseOutcome::Ok(ParsedFileEntry {
        rel,
        language,
        parsed,
        hash: sha256_hex(&bytes),
        mtime_ns: file_mtime_ns(&meta),
        size: meta.len() as i64,
    })
}

/// Write one parsed entry into an open transaction. Mirrors the old inline
/// write block in `index_file`. Returns the per-file (symbols, imports,
/// identifiers) counts so the caller can roll them into [`IndexStats`].
fn write_entry(
    tx: &rusqlite::Transaction<'_>,
    package_id: Option<&str>,
    root: &Path,
    root_str: &str,
    indexed_at: &str,
    entry: &ParsedFileEntry,
) -> Result<(usize, usize, usize)> {
    let file_str = entry.rel.to_string_lossy().into_owned();
    let pkg = package_id.unwrap_or("");

    store::replace_file(
        tx,
        pkg,
        root_str,
        &file_str,
        entry.language.as_str(),
        entry.mtime_ns,
        entry.size,
        &entry.hash,
        indexed_at,
        &entry.parsed,
    )?;
    // Resolve each raw import to a concrete file under `root` (when
    // possible) and persist as dep edges. Unresolved imports still get a
    // row with `importee_file = NULL` so the agent can see them.
    for imp in &entry.parsed.imports {
        let resolved = resolver::resolve_import(root, &entry.rel, entry.language, &imp.target);
        let importee_str = resolved.as_ref().map(|p| p.to_string_lossy().into_owned());
        store::insert_dep(
            tx,
            root_str,
            &file_str,
            importee_str.as_deref(),
            &imp.target,
            imp.line,
        )?;
    }
    Ok((
        entry.parsed.symbols.len(),
        entry.parsed.imports.len(),
        entry.parsed.identifiers.len(),
    ))
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
    let entry = match parse_one(root, rel.to_path_buf(), language) {
        ParseOutcome::Ok(e) => e,
        ParseOutcome::Skip => return Ok((0, 0, 0)),
        ParseOutcome::Err(_, e) => return Err(e),
    };
    let now = Utc::now().to_rfc3339();
    let root_str = root.to_string_lossy().into_owned();
    let tx = conn.transaction()?;
    let counts = write_entry(&tx, package_id, root, &root_str, &now, &entry)?;
    tx.commit()?;
    Ok(counts)
}

/// Apply a precomputed plan: evict removed files, then parse stale files in
/// parallel and write them serially in chunked transactions.
///
/// The shape (parallel parse → serial write) exists because tree-sitter parses
/// are CPU-bound and embarrassingly parallel, while rusqlite writes need a
/// single connection. Chunking bounds peak memory at roughly
/// `PARSE_CHUNK_SIZE` files' worth of `ParsedFile` data.
fn apply_plan(
    conn: &mut Connection,
    package_id: Option<&str>,
    root: &Path,
    plan: Plan,
) -> Result<IndexStats> {
    let mut stats = IndexStats::default();
    let root_str = root.to_string_lossy().into_owned();

    // Evict files no longer on disk first — keeps the index coherent even if
    // the parse pass below fails partway through.
    if !plan.removed.is_empty() {
        let tx = conn.transaction()?;
        for rel in &plan.removed {
            store::delete_file(&tx, &root_str, &rel.to_string_lossy())?;
        }
        tx.commit()?;
    }

    if plan.to_index.is_empty() {
        return Ok(stats);
    }

    // One-shot stderr hint when the cold-cache work is non-trivial. The
    // agent-facing complaint was "command hung for 30s" with no signal that
    // work was happening — this line answers that. Goes to stderr so it
    // doesn't pollute the structured stdout the commands emit.
    if plan.to_index.len() >= COLD_PROGRESS_THRESHOLD {
        eprintln!(
            "indexing {} files for first-time lookup (run `kcl prepare` ahead of time to avoid this)…",
            plan.to_index.len()
        );
    }

    for chunk in plan.to_index.chunks(PARSE_CHUNK_SIZE) {
        let outcomes: Vec<ParseOutcome> = chunk
            .par_iter()
            .map(|(rel, lang, _)| parse_one(root, rel.clone(), *lang))
            .collect();

        let now = Utc::now().to_rfc3339();
        let tx = conn.transaction()?;
        for outcome in outcomes {
            match outcome {
                ParseOutcome::Ok(entry) => {
                    match write_entry(&tx, package_id, root, &root_str, &now, &entry) {
                        Ok((s, i, d)) => {
                            stats.files_indexed += 1;
                            stats.symbols += s;
                            stats.imports += i;
                            stats.identifiers += d;
                        }
                        Err(e) => {
                            stats.files_failed += 1;
                            eprintln!(
                                "warning: failed to write index rows for `{}`: {:#}",
                                entry.rel.display(),
                                e
                            );
                        }
                    }
                }
                ParseOutcome::Skip => {
                    // Binary or empty-after-utf8-check; not counted as a failure.
                }
                ParseOutcome::Err(rel, e) => {
                    stats.files_failed += 1;
                    eprintln!("warning: failed to index `{}`: {:#}", rel.display(), e);
                }
            }
        }
        tx.commit()?;
    }

    Ok(stats)
}

/// Index every file the plan flags as stale. Failures are logged + skipped —
/// one bad parse must not abort the whole run. Used both by `kcl prepare`
/// (eager full-repo build) and by `kcl explore <cmd>` (lazy bring-up-to-date
/// at command time). The cold-cache progress hint inside [`apply_plan`]
/// makes the lazy path safe to invoke from a single command.
pub fn index_target(
    conn: &mut Connection,
    package_id: Option<&str>,
    root: &Path,
) -> Result<IndexStats> {
    let plan = compute_plan(conn, root)?;
    apply_plan(conn, package_id, root, plan)
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
        index_target(&mut conn, None, root).unwrap();

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

    /// Cold-cache index pass over a fixture larger than the parallel chunk
    /// size: every file gets indexed exactly once, counts roll up correctly,
    /// and the file count crosses the progress threshold. (The threshold
    /// stderr line itself isn't captured here — we exercise it from a CLI
    /// smoke test instead since `eprintln!` doesn't go through any handle
    /// we could intercept.)
    #[test]
    fn index_target_handles_more_files_than_chunk_size() {
        let dir = tempdir().unwrap();
        let root = dir.path();

        // Two chunks' worth + a partial chunk — exercises the chunk loop's
        // remainder handling. Also above COLD_PROGRESS_THRESHOLD so a real
        // CLI run would emit the hint.
        let n = (PARSE_CHUNK_SIZE * 2) + 17;
        assert!(n > COLD_PROGRESS_THRESHOLD);
        for i in 0..n {
            std::fs::write(
                root.join(format!("file_{i:04}.rs")),
                format!("pub fn func_{i}() {{}}\npub struct Type_{i};\n"),
            )
            .unwrap();
        }

        let mut conn = db::open_memory().unwrap();
        let stats = index_target(&mut conn, None, root).unwrap();
        assert_eq!(stats.files_indexed, n);
        assert_eq!(stats.files_failed, 0);
        // Each fixture file declares exactly 2 symbols.
        assert_eq!(stats.symbols, n * 2);

        let row_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM outline_files", [], |r| r.get(0))
            .unwrap();
        assert_eq!(row_count as usize, n);

        // Re-running with everything up to date is a no-op.
        let stats2 = index_target(&mut conn, None, root).unwrap();
        assert_eq!(stats2.files_indexed, 0);
        assert_eq!(stats2.files_failed, 0);
    }
}
