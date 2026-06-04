use std::collections::BTreeMap;
use std::path::Path;

use anyhow::Result;
use chrono::{DateTime, Utc};

use crate::config::Config;
use crate::db;
use crate::models::conversation::Conversation;
use crate::models::package::{Package, SourceType};
use crate::models::prepared_context::PreparedContext;
use crate::paths;

/// Delete on-disk clones of git packages whose last activity is older than
/// `days` days. Package rows, conversation rows, log directories, and prepared
/// contexts are all preserved — only the working tree is removed. A subsequent
/// `kcl ask` transparently re-clones a pruned package (see `ask::run`).
///
/// "Activity" for a package is the most recent of: its registration time, its
/// most recent conversation, and its latest prepared context. Because multiple
/// packages (e.g. monorepo subprojects) can share one on-disk clone, clones are
/// grouped by path and a group is only pruned when *every* member is stale.
pub fn run(days: u32, dry_run: bool) -> Result<()> {
    let db_path = paths::db_file()?;
    let conn = db::open(&db_path)?;

    let config = Config::load_or_default()?;
    let clone_dir = config.resolved_clone_dir()?;

    let cutoff = Utc::now() - chrono::Duration::days(days as i64);

    let packages = Package::list_all(&conn)?;

    // Group prune candidates by their on-disk path. A candidate is a git
    // package whose clone lives inside the configured clone_dir (kcl-managed —
    // we never prune a user's own `--path --git` checkout). Each group tracks
    // the latest activity across its members and the identifiers sharing it.
    let mut groups: BTreeMap<String, (DateTime<Utc>, Vec<String>)> = BTreeMap::new();
    for pkg in &packages {
        if pkg.source_type != SourceType::Git {
            continue;
        }
        if !Path::new(&pkg.path).starts_with(&clone_dir) {
            continue;
        }

        let activity = package_activity(&conn, pkg)?;
        let entry = groups
            .entry(pkg.path.clone())
            .or_insert_with(|| (activity, Vec::new()));
        if activity > entry.0 {
            entry.0 = activity;
        }
        entry.1.push(pkg.identifier.clone());
    }

    // Decide which path-groups are prunable, then act on the ones whose
    // directory still exists on disk.
    let mut pruned = 0usize;
    for (path, (group_activity, identifiers)) in &groups {
        if !is_stale(*group_activity, cutoff) {
            continue;
        }
        if !Path::new(path).exists() {
            continue;
        }

        let used_by = identifiers.join(", ");
        if dry_run {
            eprintln!(
                "would prune `{}` (used by: {}), last activity {}",
                path,
                used_by,
                group_activity.to_rfc3339()
            );
            pruned += 1;
        } else if let Err(e) = std::fs::remove_dir_all(path) {
            // Don't abort the whole batch on a single failed removal.
            eprintln!("warning: failed to prune `{}`: {}", path, e);
        } else {
            eprintln!("pruned `{}` (used by: {})", path, used_by);
            pruned += 1;
        }
    }

    if pruned == 0 {
        eprintln!("no clones to prune");
    } else {
        eprintln!(
            "{} clone(s) {}",
            pruned,
            if dry_run { "would be pruned" } else { "pruned" }
        );
    }

    Ok(())
}

/// Compute a package's last-activity timestamp: the most recent of its
/// registration time, its latest conversation, and its latest prepared context.
fn package_activity(conn: &rusqlite::Connection, pkg: &Package) -> Result<DateTime<Utc>> {
    let last_conversation =
        Conversation::last_activity_at(conn, &pkg.id)?.unwrap_or(pkg.created_at);
    let last_prepared = PreparedContext::get_latest_for_package(conn, &pkg.id)
        .ok()
        .flatten()
        .map(|p| p.created_at)
        .unwrap_or(pkg.created_at);
    Ok(pkg.created_at.max(last_conversation).max(last_prepared))
}

/// True when `activity` is strictly older than `cutoff` — i.e. the clone has
/// gone untouched long enough to be a prune candidate.
fn is_stale(activity: DateTime<Utc>, cutoff: DateTime<Utc>) -> bool {
    activity < cutoff
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_stale_true_when_activity_before_cutoff() {
        let cutoff = Utc::now();
        let old = cutoff - chrono::Duration::days(5);
        assert!(is_stale(old, cutoff));
    }

    #[test]
    fn is_stale_false_when_activity_after_cutoff() {
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let fresh = Utc::now();
        assert!(!is_stale(fresh, cutoff));
        // Exactly at the cutoff is not stale (strict `<`).
        assert!(!is_stale(cutoff, cutoff));
    }

    /// Mirror of `run`'s grouping decision, isolated so the path-grouping logic
    /// (shared clone kept while any member is fresh; pruned only when all
    /// members are stale) is unit-testable without a database or filesystem.
    /// Input: `(path, activity)` pairs for candidate packages. Output: the set
    /// of paths whose group_activity is stale relative to `cutoff`.
    fn prunable_paths(candidates: &[(&str, DateTime<Utc>)], cutoff: DateTime<Utc>) -> Vec<String> {
        let mut groups: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
        for (path, activity) in candidates {
            let entry = groups.entry(path.to_string()).or_insert(*activity);
            if *activity > *entry {
                *entry = *activity;
            }
        }
        let mut out: Vec<String> = groups
            .into_iter()
            .filter(|(_, group_activity)| is_stale(*group_activity, cutoff))
            .map(|(path, _)| path)
            .collect();
        out.sort();
        out
    }

    #[test]
    fn shared_clone_kept_when_one_member_is_fresh() {
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let stale = cutoff - chrono::Duration::days(10);
        let fresh = Utc::now();
        // Two packages share /clones/monorepo; one stale, one fresh.
        let candidates = [("/clones/monorepo", stale), ("/clones/monorepo", fresh)];
        assert!(prunable_paths(&candidates, cutoff).is_empty());
    }

    #[test]
    fn shared_clone_pruned_when_all_members_stale() {
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let stale_a = cutoff - chrono::Duration::days(10);
        let stale_b = cutoff - chrono::Duration::days(40);
        let candidates = [("/clones/monorepo", stale_a), ("/clones/monorepo", stale_b)];
        assert_eq!(
            prunable_paths(&candidates, cutoff),
            vec!["/clones/monorepo".to_string()]
        );
    }

    #[test]
    fn package_outside_clone_dir_is_never_a_candidate() {
        // The clone-dir membership filter lives in `run` (a package whose path
        // does not start with clone_dir is skipped before grouping). Mirror
        // that here: an out-of-clone-dir path simply never reaches
        // `prunable_paths`, so even a long-stale activity cannot prune it.
        let cutoff = Utc::now() - chrono::Duration::days(30);
        let clone_dir = Path::new("/clones");
        let stale = cutoff - chrono::Duration::days(100);

        // Filter exactly as `run` does, then group.
        let raw = [("/elsewhere/repo", stale), ("/clones/managed", stale)];
        let candidates: Vec<(&str, DateTime<Utc>)> = raw
            .iter()
            .filter(|(p, _)| Path::new(p).starts_with(clone_dir))
            .copied()
            .collect();

        let prunable = prunable_paths(&candidates, cutoff);
        assert_eq!(prunable, vec!["/clones/managed".to_string()]);
        assert!(!prunable.contains(&"/elsewhere/repo".to_string()));
    }
}
