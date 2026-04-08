use std::path::Path;

use anyhow::Result;
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db;
use crate::dirs;
use crate::git;
use crate::harness;
use crate::models::conversation::Conversation;
use crate::models::package::{Package, SourceType};

/// The JSON log file written to disk for each conversation.
#[derive(Debug, Serialize, Deserialize)]
struct ConversationLog {
    id: String,
    package_id: String,
    package_identifier: String,
    question: String,
    harness: String,
    started_at: String,
    finished_at: String,
    exit_code: Option<i32>,
    response: String,
}

pub fn run(
    identifier: &str,
    question: &str,
    harness_override: Option<&str>,
    timeout_override: Option<u64>,
    no_pull: bool,
    context: u32,
) -> Result<()> {
    // 1. Open DB and look up package.
    let db_path = dirs::db_file()?;
    let conn = db::open(&db_path)?;

    let pkg = Package::get_by_identifier(&conn, identifier)?
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Package '{}' not found. Run `kcl list` to see available packages.",
                identifier
            )
        })?;

    // 2. Load config to resolve harness.
    let config = Config::load_or_default()?;

    // Resolve harness name: CLI flag > package override > config default.
    let harness_name = harness_override
        .map(|s| s.to_string())
        .or_else(|| pkg.harness.clone())
        .unwrap_or_else(|| config.default_harness.clone());

    let harness_config = config
        .harnesses
        .get(&harness_name)
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Harness '{}' not found in config. Run `kcl init` to detect harnesses or `kcl config set harnesses.{}.command <path>` to add it manually.",
                harness_name,
                harness_name
            )
        })?
        .clone();

    let timeout = timeout_override.unwrap_or(config.default_timeout);

    // 3. Auto-pull if applicable.
    if !no_pull && pkg.auto_pull && pkg.source_type == SourceType::Git {
        let repo_path = Path::new(&pkg.path);
        eprintln!("pulling {} ...", pkg.identifier);
        match git::pull(repo_path) {
            Ok(msg) => eprintln!("{}: {}", pkg.identifier, msg),
            Err(e) => eprintln!("warning: pull failed for {}: {}", pkg.identifier, e),
        }
    }

    // 4. Build prompt with optional context.
    let recent_questions = if context > 0 {
        Conversation::recent_questions(&conn, &pkg.id, context)?
    } else {
        Vec::new()
    };

    let context_slice = if recent_questions.is_empty() {
        None
    } else {
        Some(recent_questions.as_slice())
    };

    let prompt = harness::build_prompt(&pkg.display_name, &pkg.identifier, question, context_slice);

    // 5. Spawn harness subprocess.
    let started_at = Utc::now();

    let rt = tokio::runtime::Runtime::new()?;
    let cwd = std::path::PathBuf::from(&pkg.path);

    let harness_result = rt.block_on(harness::run_harness(
        &harness_config,
        &prompt,
        &cwd,
        timeout,
        true, // stream stdout to caller
    ));

    let finished_at = Utc::now();

    // Handle harness execution result.
    let output = match harness_result {
        Ok(output) => output,
        Err(e) => {
            eprintln!("error: {}", e);
            std::process::exit(2);
        }
    };

    // 6. Save conversation log as JSON file.
    let conv_id = uuid::Uuid::new_v4().to_string();
    let short_id = &conv_id[..8];
    let timestamp = started_at.format("%Y%m%d-%H%M%S");
    let log_filename = format!("{}-{}.json", timestamp, short_id);
    let relative_log_path = format!("{}/{}", pkg.identifier, log_filename);

    let log_dir = dirs::log_dir()?;
    let pkg_log_dir = log_dir.join(&pkg.identifier);
    std::fs::create_dir_all(&pkg_log_dir)?;

    let log = ConversationLog {
        id: conv_id.clone(),
        package_id: pkg.id.clone(),
        package_identifier: pkg.identifier.clone(),
        question: question.to_string(),
        harness: harness_name.clone(),
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        exit_code: output.exit_code,
        response: output.stdout.clone(),
    };

    let log_path = pkg_log_dir.join(&log_filename);
    let log_json = serde_json::to_string_pretty(&log)?;
    std::fs::write(&log_path, &log_json)?;

    // 7. Insert conversation index row into SQLite.
    let conversation = Conversation {
        id: conv_id,
        package_id: pkg.id.clone(),
        question: question.to_string(),
        harness: harness_name,
        exit_code: output.exit_code,
        log_path: relative_log_path,
        created_at: started_at,
    };
    conversation.insert(&conn)?;

    // 8. Exit with harness exit code.
    let exit_code = output.exit_code.unwrap_or(1);
    if exit_code != 0 {
        std::process::exit(2);
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::package::{Package, SourceType};

    #[test]
    fn ask_nonexistent_package_returns_error() {
        let conn = db::open_memory().unwrap();
        let result = Package::get_by_identifier(&conn, "nonexistent").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn conversation_log_serializes_correctly() {
        let log = ConversationLog {
            id: "abc123".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "axum".to_string(),
            question: "How does routing work?".to_string(),
            harness: "claude".to_string(),
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: Some(0),
            response: "Routing in axum uses...".to_string(),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["id"], "abc123");
        assert_eq!(parsed["package_identifier"], "axum");
        assert_eq!(parsed["question"], "How does routing work?");
        assert_eq!(parsed["harness"], "claude");
        assert_eq!(parsed["exit_code"], 0);
        assert_eq!(parsed["response"], "Routing in axum uses...");
    }

    #[test]
    fn conversation_log_with_null_exit_code() {
        let log = ConversationLog {
            id: "abc123".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "test".to_string(),
            question: "test?".to_string(),
            harness: "claude".to_string(),
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: None,
            response: "output".to_string(),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["exit_code"].is_null());
    }

    #[test]
    fn recent_questions_with_context() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "test-ctx".to_string(),
            "Test Context".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/test-ctx".to_string(),
            false,
            None,
        );
        pkg.insert(&conn).unwrap();

        // Insert some conversations.
        for i in 0..5 {
            let conv = Conversation::new(
                pkg.id.clone(),
                format!("Question {}", i),
                "claude".to_string(),
                Some(0),
                format!("/tmp/logs/conv{}.json", i),
            );
            conv.insert(&conn).unwrap();
        }

        // Fetch recent questions with limit.
        let questions = Conversation::recent_questions(&conn, &pkg.id, 3).unwrap();
        assert_eq!(questions.len(), 3);
        // Most recent first.
        assert_eq!(questions[0], "Question 4");
        assert_eq!(questions[1], "Question 3");
        assert_eq!(questions[2], "Question 2");
    }

    #[test]
    fn recent_questions_empty_when_none_exist() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "empty-pkg".to_string(),
            "Empty Package".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/empty-pkg".to_string(),
            false,
            None,
        );
        pkg.insert(&conn).unwrap();

        let questions = Conversation::recent_questions(&conn, &pkg.id, 5).unwrap();
        assert!(questions.is_empty());
    }
}
