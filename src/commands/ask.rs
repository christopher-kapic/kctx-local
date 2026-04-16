use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db;
use crate::git;
use crate::harness;
use crate::models::conversation::Conversation;
use crate::models::package::{Package, SourceType};
use crate::paths;

/// The JSON log file written to disk for each conversation.
#[derive(Debug, Serialize, Deserialize)]
struct ConversationLog {
    id: String,
    package_id: String,
    package_identifier: String,
    question: String,
    harness: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<String>,
    started_at: String,
    finished_at: String,
    exit_code: Option<i32>,
    response: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pull_error: Option<String>,
}

/// Arguments for a single `kcl ask` invocation.
///
/// Grouped into a struct so the call site (main.rs) and `run` itself stay
/// readable as the set of knobs grows.
pub struct AskArgs<'a> {
    pub identifier: &'a str,
    pub question: &'a str,
    pub harness_override: Option<&'a str>,
    pub model: Option<&'a str>,
    pub timeout_override: Option<u64>,
    pub no_pull: bool,
    pub branch_override: Option<&'a str>,
    pub context: u32,
}

pub async fn run(args: AskArgs<'_>) -> Result<i32> {
    let AskArgs {
        identifier,
        question,
        harness_override,
        model,
        timeout_override,
        no_pull,
        branch_override,
        context,
    } = args;

    // 1. Open DB and look up package.
    let db_path = paths::db_file()?;
    let conn = db::open(&db_path)?;

    let pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package `{}` not found. Run `kcl list` to see available packages.",
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
                "Harness `{}` not found in config. Run `kcl init` to detect harnesses or `kcl config set harnesses.{}.command <path>` to add it manually.",
                harness_name,
                harness_name
            )
        })?
        .clone();

    let timeout = timeout_override.unwrap_or(config.default_timeout);
    crate::config::validate_timeout(timeout)?;

    // Resolve the effective model: CLI flag wins, otherwise fall back to the
    // harness's configured `default_model` (if any). The resolved value is
    // forwarded to the harness and recorded in the conversation log so users
    // can tell which model actually answered.
    let effective_model: Option<String> = model
        .map(|s| s.to_string())
        .or_else(|| harness_config.default_model.clone());

    // 3a. If --branch was supplied, check it out (saving the current branch
    //     so we can restore it after the harness runs). Branch overrides are
    //     only meaningful for git packages.
    let repo_path = Path::new(&pkg.path);
    let original_branch: Option<String> = if let Some(target_branch) = branch_override {
        if pkg.source_type != SourceType::Git {
            anyhow::bail!(
                "--branch is only valid for git packages; `{}` is a local package",
                pkg.identifier
            );
        }

        let current = git::current_branch(repo_path).map_err(|e| {
            anyhow::anyhow!(
                "failed to determine current branch for {}: {}",
                pkg.identifier,
                e
            )
        })?;

        if current != target_branch {
            eprintln!("checking out {} (was on {}) ...", target_branch, current);
            git::checkout(repo_path, target_branch)?;
            Some(current)
        } else {
            // Already on the requested branch — nothing to restore.
            None
        }
    } else {
        None
    };

    // 3b. Auto-pull if applicable. --no-pull always wins. When --branch is
    //     supplied we default to pulling (so the user gets the latest of
    //     that branch), but --no-pull can suppress it.
    let should_pull = if no_pull {
        false
    } else if branch_override.is_some() {
        true
    } else {
        pkg.auto_pull && pkg.source_type == SourceType::Git
    };
    let mut pull_error: Option<String> = None;
    if should_pull && pkg.source_type == SourceType::Git {
        eprintln!("pulling {} ...", pkg.identifier);
        match git::pull(repo_path) {
            Ok(msg) => eprintln!("{}: {}", pkg.identifier, msg),
            Err(e) => {
                let msg = format!("pull failed for {}: {}", pkg.identifier, e);
                eprintln!("warning: {}", msg);
                pull_error = Some(msg);
            }
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

    let cwd = std::path::PathBuf::from(&pkg.path);

    let harness_result = harness::run_harness(
        &harness_config,
        &prompt,
        &cwd,
        timeout,
        true, // stream stdout to caller
        effective_model.as_deref(),
    )
    .await;

    let finished_at = Utc::now();

    // Handle harness execution result. Regardless of success or failure we
    // persist a conversation record so every invocation appears in `kcl history`.
    let (response_text, exit_code) = match harness_result {
        Ok(output) => {
            let code = output.exit_code.unwrap_or(1);
            (output.stdout, Some(code))
        }
        Err(e) => {
            eprintln!("error: {}", e);
            (format!("[error] {}", e), None)
        }
    };

    // 6. Save conversation log as JSON file.
    let conv_id = uuid::Uuid::new_v4().to_string();
    let short_id = &conv_id[..8];
    let timestamp = started_at.format("%Y%m%d-%H%M%S");
    let log_filename = format!("{}-{}.json", timestamp, short_id);
    let relative_log_path = format!("{}/{}", pkg.identifier, log_filename);

    let log_dir = paths::log_dir()?;
    let pkg_log_dir = log_dir.join(&pkg.identifier);
    std::fs::create_dir_all(&pkg_log_dir)
        .with_context(|| format!("failed to create log directory {}", pkg_log_dir.display()))?;

    // Only record the model in the log if the harness actually accepted it.
    // (model_args being non-empty is the signal that the model was forwarded.)
    let logged_model = effective_model
        .clone()
        .filter(|_| !harness_config.model_args.is_empty());

    let log = ConversationLog {
        id: conv_id.clone(),
        package_id: pkg.id.clone(),
        package_identifier: pkg.identifier.clone(),
        question: question.to_string(),
        harness: harness_name.clone(),
        model: logged_model,
        started_at: started_at.to_rfc3339(),
        finished_at: finished_at.to_rfc3339(),
        exit_code,
        response: response_text,
        pull_error,
    };

    let log_path = pkg_log_dir.join(&log_filename);
    let log_json = serde_json::to_string_pretty(&log)?;
    std::fs::write(&log_path, &log_json)
        .with_context(|| format!("failed to write conversation log to {}", log_path.display()))?;

    // 7. Insert conversation index row into SQLite.
    let conversation = Conversation {
        id: conv_id,
        package_id: pkg.id.clone(),
        question: question.to_string(),
        harness: harness_name,
        exit_code,
        log_path: relative_log_path,
        created_at: started_at,
    };
    conversation.insert(&conn)?;

    // 8. Restore the original branch (if we changed it) and exit with the
    //    harness exit code.
    restore_branch(repo_path, original_branch.as_deref());

    match exit_code {
        Some(0) => Ok(0),
        _ => Ok(2),
    }
}

/// Try to restore `repo_path` to the previously checked-out branch.
///
/// Failures are logged to stderr but never propagated — the harness has
/// already produced its result and the user shouldn't see a successful
/// answer turn into a failed exit code just because git was unhappy.
fn restore_branch(repo_path: &Path, original_branch: Option<&str>) {
    let Some(branch) = original_branch else {
        return;
    };
    eprintln!("restoring branch {} ...", branch);
    if let Err(e) = git::checkout(repo_path, branch) {
        eprintln!("warning: failed to restore branch {}: {}", branch, e);
    }
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
            model: Some("claude-sonnet-4.6".to_string()),
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: Some(0),
            response: "Routing in axum uses...".to_string(),
            pull_error: None,
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["id"], "abc123");
        assert_eq!(parsed["package_identifier"], "axum");
        assert_eq!(parsed["question"], "How does routing work?");
        assert_eq!(parsed["harness"], "claude");
        assert_eq!(parsed["model"], "claude-sonnet-4.6");
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
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: None,
            response: "output".to_string(),
            pull_error: None,
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed["exit_code"].is_null());
        // model is omitted from the serialized JSON when None
        assert!(parsed.get("model").is_none());
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
    fn conversation_log_captures_harness_error() {
        let log = ConversationLog {
            id: "err-123".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "broken-pkg".to_string(),
            question: "Will this fail?".to_string(),
            harness: "claude".to_string(),
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:05+00:00".to_string(),
            exit_code: None,
            response: "[error] harness timed out after 120s".to_string(),
            pull_error: None,
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(parsed["id"], "err-123");
        assert_eq!(parsed["package_identifier"], "broken-pkg");
        assert!(parsed["exit_code"].is_null());
        assert!(parsed["response"].as_str().unwrap().starts_with("[error]"));
    }

    #[test]
    fn failed_harness_creates_db_record() {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "fail-pkg".to_string(),
            "Fail Package".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/fail-pkg".to_string(),
            false,
            None,
        );
        pkg.insert(&conn).unwrap();

        let conv = Conversation::new(
            pkg.id.clone(),
            "question that fails".to_string(),
            "claude".to_string(),
            None,
            "/tmp/logs/fail.json".to_string(),
        );
        conv.insert(&conn).unwrap();

        let retrieved = Conversation::get_by_id(&conn, &conv.id)
            .unwrap()
            .expect("failed conversation should be persisted");
        assert_eq!(retrieved.exit_code, None);
        assert_eq!(retrieved.question, "question that fails");
    }

    #[test]
    fn conversation_log_records_pull_error() {
        let log = ConversationLog {
            id: "pull-err-1".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "axum".to_string(),
            question: "How does routing work?".to_string(),
            harness: "claude".to_string(),
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:45+00:00".to_string(),
            exit_code: Some(0),
            response: "Routing in axum uses...".to_string(),
            pull_error: Some("pull failed for axum: remote unreachable".to_string()),
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();

        assert_eq!(
            parsed["pull_error"].as_str().unwrap(),
            "pull failed for axum: remote unreachable"
        );

        // Round-trips cleanly.
        let reparsed: ConversationLog = serde_json::from_str(&json).unwrap();
        assert_eq!(
            reparsed.pull_error.as_deref(),
            Some("pull failed for axum: remote unreachable")
        );
    }

    #[test]
    fn conversation_log_omits_pull_error_when_none() {
        let log = ConversationLog {
            id: "no-pull-err".to_string(),
            package_id: "pkg-uuid".to_string(),
            package_identifier: "axum".to_string(),
            question: "q".to_string(),
            harness: "claude".to_string(),
            model: None,
            started_at: "2026-04-07T10:30:00+00:00".to_string(),
            finished_at: "2026-04-07T10:30:01+00:00".to_string(),
            exit_code: Some(0),
            response: "ok".to_string(),
            pull_error: None,
        };

        let json = serde_json::to_string_pretty(&log).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.get("pull_error").is_none());
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
