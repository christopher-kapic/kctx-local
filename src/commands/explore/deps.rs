//! `kcl explore deps <file>` — file-level import graph.
//!
//! Walks the resolved dep edges in `outline_deps` to N hops in either
//! direction. Direction is one of `forward` (this file's imports),
//! `reverse` (files that import this one), or `both` (default).
//!
//! Cycles are handled by tracking the visited set; we report each file at
//! its *first* (shortest) hop distance only. Unresolved imports are
//! included alongside the forward results so the agent knows what couldn't
//! be resolved (external crates, broken paths, etc.).

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;

use super::util::{json_or_text, resolve_path, resolve_target};
use crate::db;
use crate::explore_index::{self, store};
use crate::paths;

/// Default hop count when the user doesn't supply `--hops`.
const DEFAULT_HOPS: usize = 1;
/// Hard upper bound — protects against accidentally walking a huge graph.
const MAX_HOPS: usize = 10;

#[derive(Debug, Serialize)]
struct HopGroup {
    hop: usize,
    paths: Vec<String>,
}

#[derive(Debug, Serialize)]
struct DepsOutput {
    file: String,
    direction: String,
    hops: usize,
    forward: Vec<HopGroup>,
    reverse: Vec<HopGroup>,
    unresolved_forward: Vec<String>,
}

pub fn run(
    file: &Path,
    hops: Option<usize>,
    direction: Option<&str>,
    package: Option<&str>,
    json: bool,
    max_bytes: usize,
) -> Result<i32> {
    let target = resolve_target(package)?;
    let abs = resolve_path(&target, file);
    if !abs.exists() {
        anyhow::bail!("File `{}` not found", abs.display());
    }
    let rel = abs.strip_prefix(&target.root).unwrap_or(&abs).to_path_buf();

    let db_path = paths::db_file()?;
    let mut conn = db::open(&db_path)?;

    // Make sure the queried file itself is indexed. For the *whole* dep
    // graph to be coherent we'd need every file indexed; cheaper to lazily
    // index the focus file and trust that earlier `prepare` / `outline`
    // runs covered the rest. `compute_plan` is fast enough to also catch
    // recently-touched files.
    explore_index::ensure_indexed(&mut conn, target.id.as_deref(), &target.root, &rel)?;
    lazy_full_pass(&mut conn, &target.root, target.id.as_deref())?;

    let dir = parse_direction(direction)?;
    let hops = hops.unwrap_or(DEFAULT_HOPS).clamp(1, MAX_HOPS);

    let root_str = target.root.to_string_lossy().into_owned();
    let file_str = rel.to_string_lossy().into_owned();

    let mut forward: Vec<HopGroup> = Vec::new();
    let mut reverse: Vec<HopGroup> = Vec::new();
    let mut unresolved_forward: Vec<String> = Vec::new();

    if matches!(dir, Direction::Forward | Direction::Both) {
        let (groups, unresolved) = bfs_forward(&conn, &root_str, &file_str, hops)?;
        forward = groups;
        unresolved_forward = unresolved;
    }
    if matches!(dir, Direction::Reverse | Direction::Both) {
        reverse = bfs_reverse(&conn, &root_str, &file_str, hops)?;
    }

    let out = DepsOutput {
        file: file_str,
        direction: direction_name(dir).to_string(),
        hops,
        forward,
        reverse,
        unresolved_forward,
    };

    json_or_text(json, &out, max_bytes, render_text)?;
    Ok(0)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Direction {
    Forward,
    Reverse,
    Both,
}

fn direction_name(d: Direction) -> &'static str {
    match d {
        Direction::Forward => "forward",
        Direction::Reverse => "reverse",
        Direction::Both => "both",
    }
}

fn parse_direction(s: Option<&str>) -> Result<Direction> {
    match s {
        None | Some("both") => Ok(Direction::Both),
        Some("forward") => Ok(Direction::Forward),
        Some("reverse") => Ok(Direction::Reverse),
        Some(other) => anyhow::bail!(
            "Invalid `--direction` value `{}`. Expected `forward`, `reverse`, or `both`.",
            other
        ),
    }
}

/// Re-index any files that are stale relative to the working tree. Failures
/// are logged and skipped — we still want a partial answer.
fn lazy_full_pass(conn: &mut Connection, root: &Path, package_id: Option<&str>) -> Result<()> {
    if let Err(e) = explore_index::index_target(conn, package_id, root) {
        eprintln!("warning: indexing failed: {:#}", e);
    }
    Ok(())
}

/// BFS forward from `start`, grouping by hop distance up to `max_hops`. Also
/// collects raw_targets that resolved to NULL for the top-level imports —
/// these become `unresolved_forward` in the output.
fn bfs_forward(
    conn: &Connection,
    root: &str,
    start: &str,
    max_hops: usize,
) -> Result<(Vec<HopGroup>, Vec<String>)> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(start.to_string());

    let mut groups: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
    frontier.push_back((start.to_string(), 0));

    let mut unresolved: Vec<String> = Vec::new();

    while let Some((cur, hop)) = frontier.pop_front() {
        if hop >= max_hops {
            continue;
        }
        let edges = store::forward_edges(conn, root, &cur)?;
        for e in edges {
            match e.importee {
                Some(importee) => {
                    if visited.insert(importee.clone()) {
                        groups.entry(hop + 1).or_default().insert(importee.clone());
                        frontier.push_back((importee, hop + 1));
                    }
                }
                None => {
                    // Only top-level unresolved targets are surfaced.
                    if hop == 0 {
                        unresolved.push(e.raw_target);
                    }
                }
            }
        }
    }

    let mut out: Vec<HopGroup> = groups
        .into_iter()
        .map(|(hop, set)| HopGroup {
            hop,
            paths: set.into_iter().collect(),
        })
        .collect();
    out.sort_by_key(|g| g.hop);
    unresolved.sort();
    unresolved.dedup();
    Ok((out, unresolved))
}

/// BFS reverse (find importers).
fn bfs_reverse(
    conn: &Connection,
    root: &str,
    start: &str,
    max_hops: usize,
) -> Result<Vec<HopGroup>> {
    let mut visited: BTreeSet<String> = BTreeSet::new();
    visited.insert(start.to_string());

    let mut groups: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    let mut frontier: VecDeque<(String, usize)> = VecDeque::new();
    frontier.push_back((start.to_string(), 0));

    while let Some((cur, hop)) = frontier.pop_front() {
        if hop >= max_hops {
            continue;
        }
        let importers = store::reverse_importers(conn, root, &cur)?;
        for imp in importers {
            if visited.insert(imp.clone()) {
                groups.entry(hop + 1).or_default().insert(imp.clone());
                frontier.push_back((imp, hop + 1));
            }
        }
    }

    let mut out: Vec<HopGroup> = groups
        .into_iter()
        .map(|(hop, set)| HopGroup {
            hop,
            paths: set.into_iter().collect(),
        })
        .collect();
    out.sort_by_key(|g| g.hop);
    Ok(out)
}

fn render_text(o: &DepsOutput) -> String {
    let mut s = String::new();
    s.push_str(&format!("{}\n", o.file));
    if matches!(o.direction.as_str(), "forward" | "both") {
        s.push_str("  imports (forward):\n");
        if o.forward.is_empty() {
            s.push_str("    (none)\n");
        } else {
            for g in &o.forward {
                s.push_str(&format!("    hop {}: {}\n", g.hop, g.paths.join(", ")));
            }
        }
        if !o.unresolved_forward.is_empty() {
            s.push_str("    unresolved:\n");
            for raw in &o.unresolved_forward {
                s.push_str(&format!("      - {}\n", raw));
            }
        }
    }
    if matches!(o.direction.as_str(), "reverse" | "both") {
        s.push_str("  imported by (reverse):\n");
        if o.reverse.is_empty() {
            s.push_str("    (none)\n");
        } else {
            for g in &o.reverse {
                s.push_str(&format!("    hop {}: {}\n", g.hop, g.paths.join(", ")));
            }
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::explore_index::Language;
    use std::path::PathBuf;
    use tempfile::tempdir;

    /// Tiny Rust fixture (3 files) so we can verify forward + reverse + both
    /// queries end-to-end through SQL.
    fn build_rust_fixture() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().to_path_buf();
        std::fs::write(root.join("Cargo.toml"), "[package]\nname = \"x\"\n").unwrap();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "use crate::a;\nuse crate::b;\nfn main(){}\n",
        )
        .unwrap();
        std::fs::write(root.join("src/a.rs"), "use crate::b;\npub fn a_thing(){}\n").unwrap();
        std::fs::write(root.join("src/b.rs"), "pub fn b_thing(){}\n").unwrap();
        (dir, root)
    }

    #[test]
    fn deps_forward_reverse_and_both_against_fixture() {
        let (_keep, root) = build_rust_fixture();
        let mut conn = db::open_memory().unwrap();
        let root_str = root.to_string_lossy().into_owned();

        // Index all three files
        for rel in ["src/main.rs", "src/a.rs", "src/b.rs"] {
            explore_index::index_file(&mut conn, None, &root, Path::new(rel), Language::Rust)
                .unwrap();
        }

        // Forward from main.rs: a.rs, b.rs at hop 1; from a.rs: b.rs already visited.
        let (fwd, _) = bfs_forward(&conn, &root_str, "src/main.rs", 2).unwrap();
        let h1: Vec<&String> = fwd
            .iter()
            .find(|g| g.hop == 1)
            .map(|g| g.paths.iter().collect())
            .unwrap();
        assert!(h1.iter().any(|p| p.ends_with("src/a.rs")));
        assert!(h1.iter().any(|p| p.ends_with("src/b.rs")));

        // Reverse from b.rs: main.rs and a.rs both import it.
        let rev = bfs_reverse(&conn, &root_str, "src/b.rs", 2).unwrap();
        let r1: Vec<&String> = rev
            .iter()
            .find(|g| g.hop == 1)
            .map(|g| g.paths.iter().collect())
            .unwrap();
        assert!(r1.iter().any(|p| p.ends_with("src/main.rs")));
        assert!(r1.iter().any(|p| p.ends_with("src/a.rs")));
    }
}
