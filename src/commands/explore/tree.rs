//! `kcl explore tree` — annotated directory tree (gitignore-aware).
//!
//! Per-file annotation: `<name>  <lang>  <lines> lines` (or `~large` for files
//! over 5 MB, which are skipped for line counting). JSON output emits an array
//! of `{path, language, lines, size_bytes, is_dir}` records.

use std::collections::{BTreeMap, HashMap};
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use anyhow::Result;
use ignore::WalkBuilder;
use serde::Serialize;

use super::util::{ExploreTarget, json_or_text, language_for, resolve_path, resolve_target};
use crate::db;
use crate::explore_index::store;
use crate::paths;

/// Files this large or larger are skipped for line counting. Annotated `~large`.
const LARGE_FILE_BYTES: u64 = 5 * 1024 * 1024;

#[derive(Debug, Serialize)]
struct TreeEntry {
    /// Path relative to the target root.
    path: String,
    /// Detected language (or `null`).
    language: Option<&'static str>,
    /// Number of lines (`null` for directories or large/unreadable files).
    lines: Option<usize>,
    /// File size in bytes (`null` for directories).
    size_bytes: Option<u64>,
    /// Indexed symbol count for this file (`null` when the file is not in the
    /// outline index — either unsupported language or hasn't been indexed yet).
    #[serde(skip_serializing_if = "Option::is_none")]
    symbols: Option<usize>,
    is_dir: bool,
}

pub fn run(
    path: Option<&Path>,
    depth: Option<usize>,
    package: Option<&str>,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let target = resolve_target(package)?;

    let start = match path {
        Some(p) => resolve_path(&target, p),
        None => target.root.clone(),
    };

    let mut builder = WalkBuilder::new(&start);
    builder
        .standard_filters(true)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .follow_links(false);
    if let Some(d) = depth {
        builder.max_depth(Some(d));
    }

    // Best-effort: pull per-file symbol counts from the index. When the DB
    // doesn't exist yet or has no rows for this root, this is just empty.
    let symbol_counts: HashMap<String, usize> = match paths::db_file() {
        Ok(p) if p.exists() => match db::open(&p) {
            Ok(conn) => {
                let root_str = target.root.to_string_lossy().into_owned();
                store::symbol_counts_by_file(&conn, &root_str).unwrap_or_default()
            }
            Err(_) => HashMap::new(),
        },
        _ => HashMap::new(),
    };

    let mut entries: Vec<TreeEntry> = Vec::new();
    for result in builder.build() {
        let dent = match result {
            Ok(d) => d,
            Err(_) => continue,
        };
        // Skip the root entry itself — listing it would clutter output.
        if dent.depth() == 0 {
            continue;
        }
        let p = dent.path();
        let rel = relative_to(&target.root, p);
        let is_dir = dent.file_type().is_some_and(|t| t.is_dir());

        let (size_bytes, lines) = if is_dir {
            (None, None)
        } else {
            let size = fs::metadata(p).ok().map(|m| m.len());
            let lines = match size {
                Some(s) if s >= LARGE_FILE_BYTES => None,
                Some(_) => count_lines(p).ok(),
                None => None,
            };
            (size, lines)
        };

        let symbols = if is_dir {
            None
        } else {
            symbol_counts.get(&rel).copied()
        };

        entries.push(TreeEntry {
            path: rel,
            language: if is_dir { None } else { language_for(p) },
            lines,
            size_bytes,
            symbols,
            is_dir,
        });
    }

    // Stable ordering for deterministic output.
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    json_or_text(json, &entries, max_bytes, |es| render_tree(&target, es))?;
    Ok(0)
}

fn relative_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

fn count_lines(p: &Path) -> std::io::Result<usize> {
    let f = fs::File::open(p)?;
    let reader = BufReader::new(f);
    let mut n = 0usize;
    for line in reader.lines() {
        // A line that fails UTF-8 still counts; non-UTF8 files have indeterminate
        // line counts so we just skip them on read error.
        if line.is_err() {
            return Ok(n);
        }
        n += 1;
    }
    Ok(n)
}

/// Render `entries` as a text tree. Entries are pre-sorted lexicographically;
/// we group by parent directory to produce an indented listing.
fn render_tree(target: &ExploreTarget, entries: &[TreeEntry]) -> String {
    // Build a tree structure for nicer indentation. Path components are joined
    // with `/` on every platform in `relative_to`.
    let mut tree: BTreeMap<PathBuf, Vec<&TreeEntry>> = BTreeMap::new();
    for e in entries {
        let parent = Path::new(&e.path)
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_default();
        tree.entry(parent).or_default().push(e);
    }

    let mut out = String::new();
    let header = match &target.id {
        Some(id) => format!("# {}\n", id),
        None => format!("# {}\n", target.root.display()),
    };
    out.push_str(&header);

    for e in entries {
        let depth = Path::new(&e.path).components().count().saturating_sub(1);
        let indent = "  ".repeat(depth);
        let name = Path::new(&e.path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| e.path.clone());

        if e.is_dir {
            out.push_str(&format!("{}{}/\n", indent, name));
            continue;
        }
        let lang = e.language.unwrap_or("-");
        let line_note = match (e.lines, e.size_bytes) {
            (Some(n), _) => format!("{} lines", n),
            (None, Some(s)) if s >= LARGE_FILE_BYTES => "~large".to_string(),
            _ => "-".to_string(),
        };
        let sym_note = e
            .symbols
            .map(|n| format!("  [{} sym]", n))
            .unwrap_or_default();
        out.push_str(&format!(
            "{}{}  {}  {}{}\n",
            indent, name, lang, line_note, sym_note
        ));
    }
    out
}
