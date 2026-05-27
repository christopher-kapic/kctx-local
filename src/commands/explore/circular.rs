//! `kcl explore circular` — circular dependency detection.
//!
//! Pulls every resolved `(importer, importee)` edge for the target's
//! `root_path` from `outline_deps`, then runs Tarjan's strongly-connected-
//! component algorithm on the resulting directed graph. Each SCC with
//! length > 1 (or a length-1 SCC that has a self-loop) is a cycle.
//!
//! Tarjan is hand-rolled (~80 LOC) to avoid a `petgraph` dependency.

use std::collections::HashMap;

use anyhow::Result;
use serde::Serialize;

use super::util::{json_or_text, resolve_target};
use crate::db;
use crate::explore_index::{self, store};
use crate::paths;

#[derive(Debug, Serialize)]
struct Cycle {
    cycle: Vec<String>,
    length: usize,
}

#[derive(Debug, Serialize)]
struct CircularOutput {
    cycles: Vec<Cycle>,
}

pub fn run(package: Option<&str>, json: bool, max_bytes: usize) -> Result<i32> {
    let target = resolve_target(package)?;
    let db_path = paths::db_file()?;
    let mut conn = db::open(&db_path)?;

    // Cycle detection wants a coherent snapshot. Reindex any stale files
    // before reading edges — otherwise we'd be reporting on an arbitrary
    // mix of old and new edges.
    let plan = explore_index::compute_plan(&conn, &target.root)?;
    let root_str = target.root.to_string_lossy().into_owned();
    if !plan.removed.is_empty() {
        let tx = conn.transaction()?;
        for rel in &plan.removed {
            store::delete_file(&tx, &root_str, &rel.to_string_lossy())?;
        }
        tx.commit()?;
    }
    for (rel, lang, _reason) in plan.to_index {
        if let Err(e) =
            explore_index::index_file(&mut conn, target.id.as_deref(), &target.root, &rel, lang)
        {
            eprintln!("warning: failed to index `{}`: {:#}", rel.display(), e);
        }
    }

    let edges = store::all_resolved_edges(&conn, &root_str)?;
    let cycles = find_cycles(&edges);

    let out = CircularOutput {
        cycles: cycles
            .into_iter()
            .map(|c| Cycle {
                length: c.len(),
                cycle: c,
            })
            .collect(),
    };

    json_or_text(json, &out, max_bytes, render_text)?;
    Ok(0)
}

fn render_text(o: &CircularOutput) -> String {
    if o.cycles.is_empty() {
        return "no circular dependencies detected\n".to_string();
    }
    let mut s = String::new();
    for (i, c) in o.cycles.iter().enumerate() {
        if i > 0 {
            s.push('\n');
        }
        // Render `A -> B -> C -> A` to make the cycle direction visible.
        let mut path = c.cycle.clone();
        if let Some(first) = path.first().cloned() {
            path.push(first);
        }
        s.push_str(&path.join(" -> "));
        s.push('\n');
    }
    s
}

/// Find all cycles via Tarjan's SCC. Each returned `Vec<String>` is one
/// cycle: an SCC of length > 1, or a length-1 SCC that has a self-loop.
fn find_cycles(edges: &[(String, String)]) -> Vec<Vec<String>> {
    // Build the graph as node-index -> list of successor node-indices.
    let mut name_to_idx: HashMap<&str, usize> = HashMap::new();
    let mut idx_to_name: Vec<String> = Vec::new();
    for (a, b) in edges {
        if !name_to_idx.contains_key(a.as_str()) {
            name_to_idx.insert(a.as_str(), idx_to_name.len());
            idx_to_name.push(a.clone());
        }
        if !name_to_idx.contains_key(b.as_str()) {
            name_to_idx.insert(b.as_str(), idx_to_name.len());
            idx_to_name.push(b.clone());
        }
    }
    let n = idx_to_name.len();
    let mut adj: Vec<Vec<usize>> = vec![Vec::new(); n];
    let mut self_loops: Vec<bool> = vec![false; n];
    for (a, b) in edges {
        let ia = name_to_idx[a.as_str()];
        let ib = name_to_idx[b.as_str()];
        adj[ia].push(ib);
        if ia == ib {
            self_loops[ia] = true;
        }
    }

    // Tarjan's SCC, iterative to avoid blowing the stack on deep graphs.
    let mut index = 0i64;
    let mut indices: Vec<i64> = vec![-1; n];
    let mut lowlink: Vec<i64> = vec![-1; n];
    let mut on_stack: Vec<bool> = vec![false; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut sccs: Vec<Vec<usize>> = Vec::new();

    // Iterative DFS state: (node, child-index-cursor)
    struct Frame {
        v: usize,
        i: usize,
    }
    let mut dfs: Vec<Frame> = Vec::new();

    for start in 0..n {
        if indices[start] != -1 {
            continue;
        }
        // Initialize start
        indices[start] = index;
        lowlink[start] = index;
        index += 1;
        stack.push(start);
        on_stack[start] = true;
        dfs.push(Frame { v: start, i: 0 });

        while let Some(top) = dfs.last_mut() {
            let v = top.v;
            if top.i < adj[v].len() {
                let w = adj[v][top.i];
                top.i += 1;
                if indices[w] == -1 {
                    indices[w] = index;
                    lowlink[w] = index;
                    index += 1;
                    stack.push(w);
                    on_stack[w] = true;
                    dfs.push(Frame { v: w, i: 0 });
                } else if on_stack[w] {
                    lowlink[v] = lowlink[v].min(indices[w]);
                }
            } else {
                // Done with v's children: maybe pop an SCC, then propagate
                // lowlink up to parent.
                if lowlink[v] == indices[v] {
                    let mut comp = Vec::new();
                    while let Some(w) = stack.pop() {
                        on_stack[w] = false;
                        comp.push(w);
                        if w == v {
                            break;
                        }
                    }
                    sccs.push(comp);
                }
                let lv = lowlink[v];
                dfs.pop();
                if let Some(parent) = dfs.last() {
                    let p = parent.v;
                    lowlink[p] = lowlink[p].min(lv);
                }
            }
        }
    }

    // Filter to cycles: SCC size > 1, or size==1 with a self-loop.
    let mut cycles: Vec<Vec<String>> = Vec::new();
    for comp in sccs {
        if comp.len() > 1 || (comp.len() == 1 && self_loops[comp[0]]) {
            let mut names: Vec<String> = comp.iter().map(|i| idx_to_name[*i].clone()).collect();
            names.sort();
            cycles.push(names);
        }
    }
    // Deterministic order: shortest cycles first, then alphabetic.
    cycles.sort_by(|a, b| a.len().cmp(&b.len()).then_with(|| a.cmp(b)));
    cycles
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_edges_no_cycles() {
        let cycles = find_cycles(&[]);
        assert!(cycles.is_empty());
    }

    #[test]
    fn dag_has_no_cycles() {
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "c".to_string()),
            ("a".to_string(), "c".to_string()),
        ];
        let cycles = find_cycles(&edges);
        assert!(cycles.is_empty());
    }

    #[test]
    fn detects_simple_two_cycle() {
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
        ];
        let cycles = find_cycles(&edges);
        assert_eq!(cycles.len(), 1);
        let c = &cycles[0];
        assert_eq!(c.len(), 2);
        assert!(c.contains(&"a".to_string()));
        assert!(c.contains(&"b".to_string()));
    }

    #[test]
    fn detects_three_cycle_and_excludes_dag_tail() {
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "c".to_string()),
            ("c".to_string(), "a".to_string()),
            ("c".to_string(), "d".to_string()), // tail, no cycle
        ];
        let cycles = find_cycles(&edges);
        assert_eq!(cycles.len(), 1);
        let c = &cycles[0];
        assert_eq!(c.len(), 3);
        assert!(c.contains(&"a".to_string()));
        assert!(c.contains(&"b".to_string()));
        assert!(c.contains(&"c".to_string()));
        assert!(!c.contains(&"d".to_string()));
    }

    #[test]
    fn detects_self_loop_as_cycle() {
        let edges = vec![("a".to_string(), "a".to_string())];
        let cycles = find_cycles(&edges);
        assert_eq!(cycles.len(), 1);
        assert_eq!(cycles[0], vec!["a".to_string()]);
    }

    #[test]
    fn detects_two_disjoint_cycles() {
        let edges = vec![
            ("a".to_string(), "b".to_string()),
            ("b".to_string(), "a".to_string()),
            ("c".to_string(), "d".to_string()),
            ("d".to_string(), "c".to_string()),
        ];
        let cycles = find_cycles(&edges);
        assert_eq!(cycles.len(), 2);
    }
}
