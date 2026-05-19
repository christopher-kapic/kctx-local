use anyhow::{Context, Result};

use crate::cli::Command;
use crate::commands::ask::ConversationLog;
use crate::db;
use crate::models::conversation::Conversation;
use crate::paths;

/// Entry point for the `kcl remember <id>` command.
/// Accepts full UUID or unique short prefix.
/// Reads the on-disk JSON log (enriched with provenance) and prints a
/// human- and agent-friendly view, or raw JSON when --json is passed.
/// All user-facing identifiers use backticks per project style.
pub fn run(cmd: &Command) -> Result<()> {
    let Command::Remember { id, json } = cmd else {
        anyhow::bail!("internal error: remember::run called with wrong command variant");
    };

    let db_path = paths::db_file()?;
    let conn = db::open(&db_path)?;

    let conv = Conversation::get_by_id_or_prefix(&conn, id)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Conversation `{}` not found. Run `kcl history list <package>` (or `kcl history show`) to list IDs, then supply a longer prefix or the full ID.",
            id
        )
    })?;

    let log_dir = paths::log_dir()?;
    let log_path = log_dir.join(&conv.log_path);

    if !log_path.exists() {
        anyhow::bail!(
            "Log file for conversation `{}` not found at {}. It may have been deleted or pruned.",
            id,
            log_path.display()
        );
    }

    let content = std::fs::read_to_string(&log_path)
        .with_context(|| format!("failed to read log file for conversation `{}`", id))?;

    if *json {
        // Agent-friendly: emit exactly the persisted log JSON.
        println!("{}", content);
        return Ok(());
    }

    // Pretty-print for humans and agents.
    // We parse the log (new fields are present on fresh logs; old logs get defaults).
    let log: ConversationLog = serde_json::from_str(&content).unwrap_or_else(|_| {
        // Fallback: synthesize from what we have if the JSON is from an older schema.
        ConversationLog {
            id: conv.id.clone(),
            package_id: conv.package_id.clone(),
            package_identifier: conv.package_id.clone(),
            question: conv.question.clone(),
            harness: conv.harness.clone(),
            model: None,
            started_at: conv.created_at.to_rfc3339(),
            finished_at: conv.created_at.to_rfc3339(),
            exit_code: conv.exit_code,
            response: "<unable to parse full log; showing index metadata only>".to_string(),
            pull_error: None,
            git_commit_sha: conv.git_commit_sha.clone(),
            git_branch: conv.git_branch.clone(),
            prepare_scope: "global".to_string(),
        }
    });

    println!("ID:                {}", log.id);
    println!("Package:           {} ({})", log.package_identifier, log.package_id);
    println!("Question:          {}", log.question);
    println!("---");
    println!("{}", log.response);
    println!("---");
    println!("Harness:           `{}`", log.harness);
    if let Some(m) = &log.model {
        println!("Model:             `{}`", m);
    }
    println!("Started:           {}", log.started_at);
    println!("Finished:          {}", log.finished_at);
    println!(
        "Exit code:         {}",
        log.exit_code
            .map(|c| c.to_string())
            .unwrap_or_else(|| "N/A".to_string())
    );
    if let Some(pe) = &log.pull_error {
        println!("Pull error:        warning: {}", pe);
    }

    println!("Provenance:");
    if let Some(sha) = &log.git_commit_sha {
        println!("  Git commit:      `{}`", sha);
    } else {
        println!("  Git commit:      (none recorded — older conversation or local package)");
    }
    if let Some(br) = &log.git_branch {
        println!("  Git branch:      `{}`", br);
    } else {
        println!("  Git branch:      (detached HEAD, unpinned, or local package)");
    }
    println!("  Prepare scope:   `{}`", log.prepare_scope);

    println!("Log file:          {}", log_path.display());

    Ok(())
}