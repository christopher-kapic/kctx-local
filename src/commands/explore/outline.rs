//! `kcl explore outline <file>` — symbols + imports for one file.
//!
//! Pulls from the persistent outline index (lazy-indexes the file if it's not
//! already current). Falls back to a tiny regex scanner for files whose
//! language we don't parse with tree-sitter.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use serde::Serialize;

use super::util::{ExploreTarget, json_or_text, resolve_path, resolve_target};
use crate::db;
use crate::explore_index::parser::{self, Language};
use crate::explore_index::{self, store};
use crate::paths;

#[derive(Debug, Serialize)]
struct OutlineOutput {
    file: String,
    language: String,
    content_hash: Option<String>,
    fallback: bool,
    imports: Vec<store::ImportRow>,
    symbols: Vec<store::SymbolRow>,
}

pub fn run(file: &Path, package: Option<&str>, json: bool, max_bytes: usize) -> Result<i32> {
    let target = resolve_target(package)?;
    let abs = resolve_path(&target, file);
    if !abs.exists() {
        anyhow::bail!("File `{}` not found", abs.display());
    }

    let rel = abs.strip_prefix(&target.root).unwrap_or(&abs).to_path_buf();

    let lang = Language::from_path(&abs).unwrap_or(Language::Unknown);

    if matches!(lang, Language::Unknown) {
        return run_fallback(&target, &abs, &rel, json, max_bytes);
    }

    let db_path = paths::db_file()?;
    let mut conn = db::open(&db_path)?;
    explore_index::ensure_indexed(&mut conn, target.id.as_deref(), &target.root, &rel)?;

    let root_str = target.root.to_string_lossy().into_owned();
    let file_str = rel.to_string_lossy().into_owned();

    let outline = store::get_outline(&conn, &root_str, &file_str)?;
    let Some(rec) = outline else {
        // ensure_indexed didn't write anything (perhaps the file is too large
        // or unreadable); fall back so the user still gets *something*.
        return run_fallback(&target, &abs, &rel, json, max_bytes);
    };

    let mut symbols = rec.symbols;
    // re-key path to the relative form the user passed
    for s in &mut symbols {
        s.path = file_str.clone();
    }

    let out = OutlineOutput {
        file: file_str.clone(),
        language: rec.language,
        content_hash: Some(rec.content_hash),
        fallback: false,
        imports: rec.imports,
        symbols,
    };

    json_or_text(json, &out, max_bytes, render_outline)?;
    Ok(0)
}

fn run_fallback(
    _target: &ExploreTarget,
    abs: &Path,
    rel: &Path,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let src = std::fs::read_to_string(abs)?;
    let parsed = parser::fallback_outline(&src);
    let symbols: Vec<store::SymbolRow> = parsed
        .symbols
        .iter()
        .map(|s| store::SymbolRow {
            path: rel.to_string_lossy().into_owned(),
            line: s.line,
            end_line: s.end_line,
            name: s.name.clone(),
            kind: s.kind.clone(),
            parent: s.parent.clone(),
            visibility: s.visibility.clone(),
            signature: s.signature.clone(),
        })
        .collect();
    let imports: Vec<store::ImportRow> = parsed
        .imports
        .iter()
        .map(|i| store::ImportRow {
            target: i.target.clone(),
            line: i.line,
        })
        .collect();

    let out = OutlineOutput {
        file: rel.to_string_lossy().into_owned(),
        language: "unknown".to_string(),
        content_hash: None,
        fallback: true,
        imports,
        symbols,
    };
    json_or_text(json, &out, max_bytes, render_outline)?;
    Ok(0)
}

fn render_outline(o: &OutlineOutput) -> String {
    let mut s = String::new();
    s.push_str(&format!("# {} ({})", o.file, o.language));
    if o.fallback {
        s.push_str("  [fallback: regex scan, install tree-sitter grammar for richer output]");
    }
    s.push('\n');

    if !o.imports.is_empty() {
        s.push_str("imports:\n");
        for i in &o.imports {
            s.push_str(&format!("  {}: {}\n", i.line, i.target));
        }
        s.push('\n');
    }

    if o.symbols.is_empty() {
        s.push_str("(no symbols)\n");
        return s;
    }

    // Group by kind for readability.
    let mut by_kind: BTreeMap<&str, Vec<&store::SymbolRow>> = BTreeMap::new();
    for sym in &o.symbols {
        by_kind.entry(sym.kind.as_str()).or_default().push(sym);
    }
    for (kind, syms) in by_kind {
        s.push_str(&format!("{}s:\n", kind));
        for sym in syms {
            let vis = sym
                .visibility
                .as_deref()
                .map(|v| format!(" [{}]", v))
                .unwrap_or_default();
            let parent = sym
                .parent
                .as_deref()
                .map(|p| format!(" (in `{}`)", p))
                .unwrap_or_default();
            let sig = sym
                .signature
                .as_deref()
                .map(|sg| format!(" — {}", sg))
                .unwrap_or_default();
            s.push_str(&format!(
                "  {}: {}{}{}{}\n",
                sym.line, sym.name, parent, vis, sig
            ));
        }
    }
    s
}
