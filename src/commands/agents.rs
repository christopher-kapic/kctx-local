//! `kcl agents [topic]` — agent-oriented usage guidance.
//!
//! Prints terse, copy-pasteable instructions aimed at a coding agent that is
//! *calling* kcl (not the harness kcl spawns). The bare command prints an
//! overview and points at the per-area topics; each topic prints focused
//! instructions for one part of the CLI. All output goes to stdout (it is the
//! command's primary product, like `kcl --help`).

use anyhow::Result;

use crate::cli::AgentsTopic;

const OVERVIEW: &str = "\
kcl — local code knowledge for agents

kcl answers questions about codebases on this machine by running a coding
harness against a local clone. Ask a question and get an answer — you do not
need to read or clone the target codebase yourself.

How to ask:
  kcl ask <package> \"<question>\"

See what you can ask about:
  kcl list                 # identifiers, one per line (add --json for detail)
  kcl packages show <id>   # details for one package

Useful `kcl ask` flags:
  --context <N>   prepend summaries of the last N Q&As for this package
  --branch <b>    answer against branch <b> (git packages only)
  --timeout <s>   override the harness timeout, in seconds
  --harness <h>   override which harness answers this one question

The answer is printed to stdout; status/progress goes to stderr. Exit codes:
  0 ok · 1 kcl error · 2 harness killed/timed out · 3 harness exited non-zero.

More agent guidance for specific areas:
  kcl agents add      registering packages
  kcl agents remove   removing packages
  kcl agents prune    reclaiming disk for clones you rarely use
  kcl agents config   what to do when kcl itself fails
";

const ADD: &str = "\
Registering packages — `kcl packages add <id> ...`

Git repo (kcl clones and manages the checkout for you):
  kcl packages add <id> --git <url> [--branch <b>] [--shallow]

Local directory (kcl points at it in place and never modifies or deletes it):
  kcl packages add <id> --path /abs/path
  kcl packages add <id> --current-path          # uses the current directory

Existing on-disk checkout tracked against its remote:
  kcl packages add <id> --git <url> --path /abs/path

Notes:
- <id> may contain letters, digits, and `-` `_` `.` `/` `@` (e.g. `@scope/name`).
- --shallow clones at depth 1: saves disk, but truncates git history.
- Re-adding the same git URL under a new <id> reuses the existing clone, so a
  monorepo can be registered under several identifiers without re-cloning.
- After adding a git package, `kcl prepare <id>` builds a compact orientation
  map that speeds up later asks (optional).
";

const REMOVE: &str = "\
Removing packages — `kcl packages remove <id>` (alias: `kcl packages rm <id>`)

This de-registers the package and deletes its conversation history and logs.
The on-disk clone is deleted ONLY when kcl created it (it lives inside the
configured clone dir) and no other package still shares that clone.

- Local `--path` packages: only de-registered. Their directory is never deleted.
- To free disk WITHOUT forgetting a package, use `kcl prune` instead — it
  deletes only the clone and transparently re-clones on the next ask. See
  `kcl agents prune`.
";

const PRUNE: &str = "\
Reclaiming disk — `kcl prune [--days <N>] [--dry-run]`

Deletes the on-disk clone of any git package not asked about in the last N
days (default 30). It keeps the package, its history, and its prepared map —
only the working copy on disk is removed.

- Only clones kcl created (inside the clone dir) are eligible. Local `--path`
  packages and your own checkouts are never touched.
- Clones shared by several packages are pruned only when ALL of them are stale.
- The next `kcl ask` against a pruned package re-clones it automatically
  (shallow, for speed) before answering — you do not have to do anything. A
  re-cloned package has truncated history; the answering agent is told it may
  run `git fetch --unshallow` if it needs older history or another version.
- Run with --dry-run first to preview what would be freed.
";

const CONFIG: &str = "\
When kcl fails — config / harness issues

kcl runs an external coding harness (Claude Code, opencode, copilot, ...).
Most failures are setup problems, not a problem with your question:

- \"Harness `X` not found in config\" → the harness is not set up. Tell the
  user to run `kcl init` (it auto-detects installed harnesses).
- \"Package `X` not found\" → run `kcl list` for valid identifiers.
- Exit 2 (killed / timed out) → retry, or suggest a larger `--timeout`.
- Exit 3 (harness exited non-zero) → the harness itself errored; read its
  message. The user may need to authenticate the harness CLI first.

IMPORTANT: Do not read, open, or modify the user's kcl configuration —
neither the files (e.g. `~/.config/kcl/config.json` and anything under the
kcl config/data directories) nor via `kcl config show|edit|set` — without
the user's explicit permission. Diagnose from the failing command's own
output, and recommend that the user run `kcl init` or the relevant
`kcl config` command themselves rather than doing it for them.
";

/// Print the overview, or the guidance for a specific topic.
pub fn run(topic: Option<AgentsTopic>) -> Result<()> {
    let text = match topic {
        None => OVERVIEW,
        Some(AgentsTopic::Add) => ADD,
        Some(AgentsTopic::Remove) => REMOVE,
        Some(AgentsTopic::Prune) => PRUNE,
        Some(AgentsTopic::Config) => CONFIG,
    };
    print!("{text}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_points_at_every_topic() {
        // The overview must advertise each topic so agents can discover them.
        for topic in [
            "agents add",
            "agents remove",
            "agents prune",
            "agents config",
        ] {
            assert!(
                OVERVIEW.contains(topic),
                "overview should mention `{topic}`"
            );
        }
        assert!(OVERVIEW.contains("kcl ask <package>"));
    }

    #[test]
    fn config_topic_warns_against_touching_user_config() {
        // The standing instruction: never read/modify user config without
        // explicit permission. Guard the key phrases so the warning can't be
        // silently dropped in a future edit.
        assert!(CONFIG.contains("without"));
        assert!(CONFIG.contains("explicit permission"));
        assert!(CONFIG.contains("config.json"));
        assert!(CONFIG.contains("kcl init"));
    }

    #[test]
    fn remove_mentions_rm_alias_and_prune_alternative() {
        assert!(REMOVE.contains("kcl packages rm"));
        assert!(REMOVE.contains("kcl prune"));
    }

    #[test]
    fn every_topic_renders_nonempty() {
        for t in [
            None,
            Some(AgentsTopic::Add),
            Some(AgentsTopic::Remove),
            Some(AgentsTopic::Prune),
            Some(AgentsTopic::Config),
        ] {
            // run() returns Ok and the backing text is non-empty.
            assert!(run(t).is_ok());
        }
        assert!(!OVERVIEW.is_empty() && !ADD.is_empty() && !PRUNE.is_empty());
    }
}
