//! `kcl explore hot` — most-recently-modified files (gitignore-aware).

use std::path::Path;
use std::time::SystemTime;

use anyhow::Result;
use chrono::{DateTime, Utc};
use ignore::WalkBuilder;
use serde::Serialize;

use super::util::{json_or_text, resolve_target};

#[derive(Debug, Serialize)]
struct HotEntry {
    path: String,
    mtime_rfc3339: String,
    size_bytes: u64,
}

pub fn run(limit: usize, package: Option<&str>, json: bool, max_bytes: usize) -> Result<i32> {
    let target = resolve_target(package)?;

    let mut builder = WalkBuilder::new(&target.root);
    builder
        .standard_filters(true)
        .hidden(true)
        .git_ignore(true)
        .git_exclude(true)
        .git_global(true)
        .parents(true)
        .follow_links(false);

    let mut rows: Vec<(SystemTime, u64, String)> = Vec::new();
    for result in builder.build() {
        let dent = match result {
            Ok(d) => d,
            Err(_) => continue,
        };
        if dent.file_type().is_some_and(|t| t.is_dir()) {
            continue;
        }
        if dent.depth() == 0 {
            continue;
        }
        let p = dent.path();
        let md = match std::fs::metadata(p) {
            Ok(m) => m,
            Err(_) => continue,
        };
        let mtime = match md.modified() {
            Ok(t) => t,
            Err(_) => continue,
        };
        let size = md.len();
        let rel = relative_to(&target.root, p);
        rows.push((mtime, size, rel));
    }

    // Sort by mtime descending.
    rows.sort_by_key(|r| std::cmp::Reverse(r.0));

    let truncated: Vec<HotEntry> = rows
        .into_iter()
        .take(limit)
        .map(|(mtime, size, path)| {
            let dt: DateTime<Utc> = mtime.into();
            HotEntry {
                path,
                mtime_rfc3339: dt.to_rfc3339(),
                size_bytes: size,
            }
        })
        .collect();

    json_or_text(json, &truncated, max_bytes, render_text)?;
    Ok(0)
}

fn relative_to(root: &Path, p: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

fn render_text(rows: &Vec<HotEntry>) -> String {
    // Aligned columns: mtime  size  path
    let mut s = String::new();
    if rows.is_empty() {
        return s;
    }
    let mtime_w = rows
        .iter()
        .map(|r| r.mtime_rfc3339.len())
        .max()
        .unwrap_or(0);
    let size_w = rows
        .iter()
        .map(|r| r.size_bytes.to_string().len())
        .max()
        .unwrap_or(0);
    for r in rows {
        s.push_str(&format!(
            "{:<mw$}  {:>sw$}  {}\n",
            r.mtime_rfc3339,
            r.size_bytes,
            r.path,
            mw = mtime_w,
            sw = size_w
        ));
    }
    s
}
