use anyhow::{Context, Result};
use serde::Serialize;

use crate::cli::HistoryCommand;
use crate::db;
use crate::models::conversation::Conversation;
use crate::models::package::Package;
use crate::paths;

/// A human-friendly summary of a conversation for list output.
#[derive(Debug, Serialize)]
struct ConversationSummary {
    id: String,
    question: String,
    harness: String,
    exit_code: Option<i32>,
    created_at: String,
}

impl From<&Conversation> for ConversationSummary {
    fn from(conv: &Conversation) -> Self {
        Self {
            id: conv.id.clone(),
            question: conv.question.clone(),
            harness: conv.harness.clone(),
            exit_code: conv.exit_code,
            created_at: conv.created_at.to_rfc3339(),
        }
    }
}

pub fn run(command: &HistoryCommand) -> Result<()> {
    match command {
        HistoryCommand::List {
            identifier,
            since,
            limit,
            json,
        } => run_list(identifier, *since, *limit, *json),
        HistoryCommand::Show { id, json } => run_show(id, *json),
    }
}

fn run_list(identifier: &str, since: Option<u32>, limit: u32, json: bool) -> Result<()> {
    let db_path = paths::db_file()?;
    let conn = db::open(&db_path)?;

    let pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package `{}` not found. Run `kcl list` to see available packages.",
            identifier
        )
    })?;

    let conversations = Conversation::list_filtered(&conn, &pkg.id, limit, since)?;

    if json {
        let summaries: Vec<ConversationSummary> = conversations
            .iter()
            .map(ConversationSummary::from)
            .collect();
        let output = serde_json::to_string_pretty(&summaries)?;
        println!("{}", output);
    } else {
        if conversations.is_empty() {
            println!("No conversations found for `{}`.", identifier);
            return Ok(());
        }

        for conv in &conversations {
            let exit_str = match conv.exit_code {
                Some(code) => format!("exit:{}", code),
                None => "exit:?".to_string(),
            };
            let ts = conv.created_at.format("%Y-%m-%d %H:%M:%S");
            // Truncate long questions for display.
            let question_display = if conv.question.chars().count() > 80 {
                format!("{}...", conv.question.chars().take(77).collect::<String>())
            } else {
                conv.question.clone()
            };
            println!(
                "{} [{}] [{}] [{}] {}",
                conv.id, ts, conv.harness, exit_str, question_display
            );
        }
    }

    Ok(())
}

fn run_show(id: &str, json: bool) -> Result<()> {
    let db_path = paths::db_file()?;
    let conn = db::open(&db_path)?;

    let conv = Conversation::get_by_id(&conn, id)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Conversation `{}` not found. Run `kcl history list <package>` to see available conversations.",
                id
            )
        })?;

    // Resolve the full log path.
    let log_dir = paths::log_dir()?;
    let log_path = log_dir.join(&conv.log_path);

    if !log_path.exists() {
        anyhow::bail!(
            "Log file not found at {}. It may have been deleted.",
            log_path.display()
        );
    }

    let content = std::fs::read_to_string(&log_path)
        .with_context(|| format!("failed to read log file: {}", log_path.display()))?;

    if json {
        // Output the raw JSON log file contents.
        println!("{}", content);
        return Ok(());
    }

    // Try to parse and print just the response for human-friendly output,
    // but fall back to printing the full JSON if parsing fails.
    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(&content) {
        if let Some(response) = parsed.get("response").and_then(|v| v.as_str()) {
            // Print metadata header then the response.
            println!("ID:        {}", conv.id);
            println!("Package:   {}", conv.package_id);
            println!("Question:  {}", conv.question);
            println!("Harness:   {}", conv.harness);
            println!(
                "Exit code: {}",
                conv.exit_code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "N/A".to_string())
            );
            println!("Time:      {}", conv.created_at.to_rfc3339());
            if let Some(pull_err) = parsed.get("pull_error").and_then(|v| v.as_str()) {
                println!("Pull:      warning: {}", pull_err);
            }
            println!("---");
            println!("{}", response);
        } else {
            // No response field — print full JSON.
            println!("{}", content);
        }
    } else {
        // Not valid JSON — print raw content.
        println!("{}", content);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::package::{Package, SourceType};

    fn setup_test_db() -> (rusqlite::Connection, Package) {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "test-pkg".to_string(),
            "Test Package".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/test-pkg".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        pkg.insert(&conn).unwrap();
        (conn, pkg)
    }

    #[test]
    fn list_filtered_respects_limit() {
        let (conn, pkg) = setup_test_db();

        for i in 0..10 {
            let conv = Conversation::new(
                pkg.id.clone(),
                format!("Question {}", i),
                "claude".to_string(),
                Some(0),
                format!("/tmp/logs/conv{}.json", i),
            None,
            None,
            );
            conv.insert(&conn).unwrap();
        }

        let results = Conversation::list_filtered(&conn, &pkg.id, 5, None).unwrap();
        assert_eq!(results.len(), 5);
        // Most recent first.
        assert_eq!(results[0].question, "Question 9");
        assert_eq!(results[4].question, "Question 5");
    }

    #[test]
    fn list_filtered_returns_all_when_limit_exceeds_count() {
        let (conn, pkg) = setup_test_db();

        for i in 0..3 {
            let conv = Conversation::new(
                pkg.id.clone(),
                format!("Question {}", i),
                "claude".to_string(),
                Some(0),
                format!("/tmp/logs/conv{}.json", i),
            None,
            None,
            );
            conv.insert(&conn).unwrap();
        }

        let results = Conversation::list_filtered(&conn, &pkg.id, 20, None).unwrap();
        assert_eq!(results.len(), 3);
    }

    #[test]
    fn list_filtered_empty_package() {
        let (conn, pkg) = setup_test_db();
        let results = Conversation::list_filtered(&conn, &pkg.id, 20, None).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn conversation_summary_from_conversation() {
        let conv = Conversation::new(
            "pkg-id".to_string(),
            "How does X work?".to_string(),
            "claude".to_string(),
            Some(0),
            "/tmp/log.json".to_string(),
            None,
            None,
        );

        let summary = ConversationSummary::from(&conv);
        assert_eq!(summary.id, conv.id);
        assert_eq!(summary.question, "How does X work?");
        assert_eq!(summary.harness, "claude");
        assert_eq!(summary.exit_code, Some(0));
    }

    #[test]
    fn conversation_summary_serializes_to_json() {
        let conv = Conversation::new(
            "pkg-id".to_string(),
            "Test question".to_string(),
            "copilot".to_string(),
            None,
            "/tmp/log.json".to_string(),
            None,
            None,
        );

        let summary = ConversationSummary::from(&conv);
        let json = serde_json::to_string(&summary).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["question"], "Test question");
        assert_eq!(parsed["harness"], "copilot");
        assert!(parsed["exit_code"].is_null());
    }

    #[test]
    fn truncate_multibyte_question_does_not_panic() {
        // 90 emoji characters — each is 4 bytes, so byte-indexing at 77 would split a char.
        let long_question = "🦀".repeat(90);
        assert!(long_question.len() > 80);
        assert!(long_question.chars().count() > 80);

        let display = if long_question.chars().count() > 80 {
            format!("{}...", long_question.chars().take(77).collect::<String>())
        } else {
            long_question.clone()
        };

        assert_eq!(display.chars().count(), 80); // 77 crabs + 3 dots
        assert!(display.ends_with("..."));
    }

    #[test]
    fn get_by_id_returns_none_for_missing() {
        let (conn, _pkg) = setup_test_db();
        let result = Conversation::get_by_id(&conn, "nonexistent-id").unwrap();
        assert!(result.is_none());
    }
}
