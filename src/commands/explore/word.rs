//! `kcl explore word <token>` — exact-identifier inverted index lookup.

use std::collections::BTreeMap;

use anyhow::Result;
use serde::Serialize;

use super::util::{json_or_text, resolve_target};
use crate::db;
use crate::explore_index::{self, store};
use crate::paths;

#[derive(Debug, Serialize)]
struct WordOutput {
    token: String,
    ignore_case: bool,
    total: usize,
    files: usize,
    hits: Vec<store::IdentifierHit>,
}

pub fn run(
    token: &str,
    ignore_case: bool,
    package: Option<&str>,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let target = resolve_target(package)?;
    let db_path = paths::db_file()?;
    let mut conn = db::open(&db_path)?;

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
    let hits = store::query_identifier(&conn, &root_str, token, ignore_case)?;

    let mut files: std::collections::HashSet<&str> = std::collections::HashSet::new();
    for h in &hits {
        files.insert(h.path.as_str());
    }

    let out = WordOutput {
        token: token.to_string(),
        ignore_case,
        total: hits.len(),
        files: files.len(),
        hits,
    };
    json_or_text(json, &out, max_bytes, render)?;
    Ok(0)
}

fn render(o: &WordOutput) -> String {
    let mut s = String::new();
    s.push_str(&format!(
        "# word `{}` ({} hits across {} files)\n",
        o.token, o.total, o.files
    ));

    if o.hits.is_empty() {
        s.push_str("(no matches)\n");
        return s;
    }

    // Group by file for a more skimmable listing while still producing one
    // line per hit (so callers can `grep`).
    let mut by_file: BTreeMap<&str, Vec<u32>> = BTreeMap::new();
    for h in &o.hits {
        by_file.entry(h.path.as_str()).or_default().push(h.line);
    }
    for (path, lines) in by_file {
        for line in lines {
            s.push_str(&format!("{}:{}\n", path, line));
        }
    }
    s
}
