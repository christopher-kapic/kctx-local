//! `kcl explore impact <symbol>` — symbol-level blast radius.
//!
//! Lazy-indexes any stale files, then:
//! 1. Finds every definition of `<symbol>` in `outline_symbols` (printed
//!    first as "definitions").
//! 2. Finds direct callers from `outline_callsites WHERE callee_name = ?`.
//! 3. For `--hops N` (default 1, cap 5), treats each direct caller's
//!    `caller_symbol` as a new query target and walks transitively.
//!
//! The walk is intentionally name-based: it just chains callee_name ->
//! caller_symbol. Same-name collisions are surfaced as candidate sites for
//! the agent/user to filter.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use super::util::{json_or_text, resolve_target};
use crate::db;
use crate::explore_index::{self, store};
use crate::paths;

const DEFAULT_HOPS: usize = 1;
const MAX_HOPS: usize = 5;

#[derive(Debug, Serialize)]
struct HopSites {
    hop: usize,
    sites: Vec<store::CallsiteRow>,
}

#[derive(Debug, Serialize)]
struct ImpactOutput {
    symbol: String,
    definitions: Vec<store::SymbolRow>,
    references_by_hop: Vec<HopSites>,
    /// When `--file` was passed: the relative scope it resolved to.
    #[serde(skip_serializing_if = "Option::is_none")]
    file_scope: Option<String>,
    /// Plain-language note surfaced when the symbol has multiple definitions
    /// or when `--file` filtered out matches — name-based matching cannot
    /// disambiguate beyond what these flags allow.
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

pub fn run(
    symbol: &str,
    hops: Option<usize>,
    file: Option<&Path>,
    package: Option<&str>,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let target = resolve_target(package)?;
    let db_path = paths::db_file()?;
    let mut conn = db::open(&db_path)?;

    // Bring stale files up to date so the impact answer reflects the
    // current working tree.
    if let Err(e) = explore_index::index_target(&mut conn, target.id.as_deref(), &target.root) {
        eprintln!("warning: indexing failed: {:#}", e);
    }
    let root_str = target.root.to_string_lossy().into_owned();

    let hops = hops.unwrap_or(DEFAULT_HOPS).clamp(1, MAX_HOPS);

    // Resolve the optional `--file` scope to a relative path string. Anything
    // outside `target.root` is ignored with a warning so we never silently
    // return nothing because the user pointed at the wrong tree.
    let file_scope = match file {
        Some(p) => {
            let abs = super::util::resolve_path(&target, p);
            match abs.strip_prefix(&target.root) {
                Ok(rel) => Some(rel.to_string_lossy().replace('\\', "/")),
                Err(_) => {
                    eprintln!(
                        "warning: `--file {}` is outside the package root `{}`; ignoring scope",
                        p.display(),
                        target.root.display()
                    );
                    None
                }
            }
        }
        None => None,
    };

    let definitions = store::query_symbols(&conn, &root_str, symbol, false, None)?;
    let refs_all = walk_impact(&conn, &root_str, symbol, hops)?;
    let (refs, dropped) = apply_file_scope(refs_all, file_scope.as_deref());

    let note = build_note(&definitions, file_scope.as_deref(), dropped);

    let out = ImpactOutput {
        symbol: symbol.to_string(),
        definitions,
        references_by_hop: refs,
        file_scope,
        note,
    };

    json_or_text(json, &out, max_bytes, render_text)?;
    Ok(0)
}

/// Drop any callsites whose `caller_file` is not the scope path or a descendant
/// of it. Path comparison is string-prefix on a `/`-normalised relative path —
/// the index already stores forward-slashed relative paths.
fn apply_file_scope(refs: Vec<HopSites>, scope: Option<&str>) -> (Vec<HopSites>, usize) {
    let Some(scope) = scope else {
        return (refs, 0);
    };
    let mut dropped = 0usize;
    let kept: Vec<HopSites> = refs
        .into_iter()
        .map(|h| {
            let sites: Vec<store::CallsiteRow> = h
                .sites
                .into_iter()
                .filter(|s| {
                    let f = s.caller_file.as_str();
                    let in_scope = f == scope
                        || f.strip_prefix(scope)
                            .is_some_and(|rest| rest.starts_with('/'));
                    if !in_scope {
                        dropped += 1;
                    }
                    in_scope
                })
                .collect();
            HopSites { hop: h.hop, sites }
        })
        .filter(|h| !h.sites.is_empty())
        .collect();
    (kept, dropped)
}

fn build_note(
    definitions: &[store::SymbolRow],
    scope: Option<&str>,
    dropped: usize,
) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if definitions.len() > 1 {
        parts.push(format!(
            "{} definitions of this name exist; name-based callsite matching may include unrelated references",
            definitions.len()
        ));
    }
    if scope.is_some() && dropped > 0 {
        parts.push(format!("{} references dropped by `--file` scope", dropped));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join("; "))
    }
}

/// BFS over `outline_callsites`: at each hop, all rows with
/// `callee_name = current_target`; the next hop's targets are the
/// distinct `caller_symbol`s discovered (skipping None).
fn walk_impact(
    conn: &Connection,
    root: &str,
    start_symbol: &str,
    max_hops: usize,
) -> Result<Vec<HopSites>> {
    let mut out: Vec<HopSites> = Vec::new();
    let mut visited_targets: BTreeSet<String> = BTreeSet::new();
    let mut visited_sites: BTreeSet<(String, u32, String)> = BTreeSet::new();
    let mut frontier: Vec<String> = vec![start_symbol.to_string()];
    visited_targets.insert(start_symbol.to_string());

    for hop in 1..=max_hops {
        let mut next_targets: BTreeSet<String> = BTreeSet::new();
        let mut hop_sites: Vec<store::CallsiteRow> = Vec::new();
        for target in &frontier {
            let rows = store::query_callsites(conn, root, target)?;
            for r in rows {
                let key = (r.caller_file.clone(), r.caller_line, r.callee_name.clone());
                if !visited_sites.insert(key) {
                    continue;
                }
                if let Some(sym) = &r.caller_symbol
                    && visited_targets.insert(sym.clone())
                {
                    next_targets.insert(sym.clone());
                }
                hop_sites.push(r);
            }
        }
        if hop_sites.is_empty() {
            break;
        }
        // Stable ordering: by file then line.
        hop_sites.sort_by(|a, b| {
            a.caller_file
                .cmp(&b.caller_file)
                .then_with(|| a.caller_line.cmp(&b.caller_line))
        });
        out.push(HopSites {
            hop,
            sites: hop_sites,
        });
        if next_targets.is_empty() {
            break;
        }
        frontier = next_targets.into_iter().collect();
    }

    Ok(out)
}

fn render_text(o: &ImpactOutput) -> String {
    let mut s = String::new();
    s.push_str(&format!("impact: {}\n", o.symbol));
    if let Some(scope) = &o.file_scope {
        s.push_str(&format!("  (scoped to `{}`)\n", scope));
    }
    if let Some(note) = &o.note {
        s.push_str(&format!("  note: {}\n", note));
    }

    s.push_str("  defined at:\n");
    if o.definitions.is_empty() {
        s.push_str("    (no definitions found in this package)\n");
    } else {
        // Group definitions by file for compact display.
        let mut by_file: BTreeMap<String, Vec<&store::SymbolRow>> = BTreeMap::new();
        for d in &o.definitions {
            by_file.entry(d.path.clone()).or_default().push(d);
        }
        for (file, defs) in by_file {
            for d in defs {
                let vis = d
                    .visibility
                    .as_deref()
                    .map(|v| format!(" {}", v))
                    .unwrap_or_default();
                s.push_str(&format!(
                    "    {}:{}  {} {}{}\n",
                    file, d.line, d.kind, d.name, vis
                ));
            }
        }
    }

    if o.references_by_hop.is_empty() {
        s.push_str("  (no references found)\n");
        return s;
    }

    for g in &o.references_by_hop {
        s.push_str(&format!(
            "  hop {} ({} reference{}):\n",
            g.hop,
            g.sites.len(),
            if g.sites.len() == 1 { "" } else { "s" }
        ));
        for site in &g.sites {
            let in_sym = site
                .caller_symbol
                .as_deref()
                .map(|n| format!("  in `{}`", n))
                .unwrap_or_default();
            s.push_str(&format!(
                "    {}:{}  [{}]{}\n",
                site.caller_file, site.caller_line, site.callee_kind, in_sym
            ));
        }
    }
    s
}
