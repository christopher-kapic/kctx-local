//! `kcl explore symbol <name>` — find a symbol's definition sites.
//!
//! Lazy-indexes any files that are stale or unknown to the index before
//! querying, then returns matching rows sorted by `path:line`.

use anyhow::Result;
use serde::Serialize;

use super::util::{json_or_text, resolve_target};
use crate::db;
use crate::explore_index::{self, store};
use crate::paths;

#[derive(Debug, Serialize)]
struct SymbolOutput {
    query: String,
    prefix: bool,
    kind: Option<String>,
    results: Vec<store::SymbolRow>,
}

pub fn run(
    name: &str,
    prefix: bool,
    kind: Option<&str>,
    package: Option<&str>,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let target = resolve_target(package)?;
    let db_path = paths::db_file()?;
    let mut conn = db::open(&db_path)?;

    // Cheap-ish: bring stale files up to date before the query. We DO NOT do
    // a full eager pass on first run — that's reserved for `kcl prepare`.
    let plan = explore_index::compute_plan(&conn, &target.root)?;
    if !plan.to_index.is_empty() {
        for (rel, lang, _) in plan.to_index {
            if let Err(e) =
                explore_index::index_file(&mut conn, target.id.as_deref(), &target.root, &rel, lang)
            {
                eprintln!("warning: failed to index `{}`: {:#}", rel.display(), e);
            }
        }
    }

    let root_str = target.root.to_string_lossy().into_owned();
    let results = store::query_symbols(&conn, &root_str, name, prefix, kind)?;

    let out = SymbolOutput {
        query: name.to_string(),
        prefix,
        kind: kind.map(|s| s.to_string()),
        results,
    };

    json_or_text(json, &out, max_bytes, render)?;
    Ok(0)
}

fn render(o: &SymbolOutput) -> String {
    let mut s = String::new();
    let q_header = if o.prefix {
        format!("# symbol prefix `{}`", o.query)
    } else {
        format!("# symbol `{}`", o.query)
    };
    s.push_str(&q_header);
    if let Some(k) = &o.kind {
        s.push_str(&format!(" (kind = {})", k));
    }
    s.push('\n');

    if o.results.is_empty() {
        s.push_str("(no matches)\n");
        return s;
    }

    for sym in &o.results {
        let parent = sym
            .parent
            .as_deref()
            .map(|p| format!(" [in `{}`]", p))
            .unwrap_or_default();
        let sig = sym
            .signature
            .as_deref()
            .map(|sg| format!(" — {}", sg))
            .unwrap_or_default();
        s.push_str(&format!(
            "{}:{}  {}  {}{}{}\n",
            sym.path, sym.line, sym.kind, sym.name, parent, sig
        ));
    }
    s
}
