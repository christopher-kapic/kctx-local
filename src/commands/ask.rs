use std::path::Path;

use anyhow::{Context, Result};
use chrono::Utc;
use serde::{Deserialize, Serialize};

use crate::config::Config;
use crate::db;
use crate::git;
use crate::git::HeadState;
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

    // Drop the connection below once we've finished all DB reads (step 4) so
    // it isn't held open for the duration of the harness run, which could
    // otherwise block concurrent writers.

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

    // 3a. Resolve the effective branch to check out. CLI `--branch` wins;
    //     otherwise fall back to the package's pinned `source_branch` (set
    //     when the package was added with `--branch`). Local packages have
    //     `source_branch == None`, so this naturally never fires for them.
    //
    //     The `--branch is only valid for git packages` error must only be
    //     raised when the user *explicitly* passed `--branch` for a non-git
    //     package — not when the branch comes from `pkg.source_branch`.
    if branch_override.is_some() && pkg.source_type != SourceType::Git {
        anyhow::bail!(
            "`--branch` is only valid for git packages; `{}` is a local package",
            pkg.identifier
        );
    }
    let effective_branch: Option<&str> =
        resolve_target_branch(branch_override, pkg.source_branch.as_deref());

    // If an effective branch is resolved AND it differs from the package's
    // current branch, check it out (saving the current HEAD state so we can
    // restore it after the harness runs, including the case where the
    // original HEAD was detached).
    let repo_path = Path::new(&pkg.path);
    let original_head: Option<HeadState> = if let Some(target_branch) = effective_branch {
        let current = git::current_head(repo_path).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to determine current HEAD for `{}`: {}",
                pkg.identifier,
                e
            )
        })?;

        // If we're already on the requested branch there's nothing to do —
        // and nothing to restore. Detached HEAD always needs a checkout.
        let already_on_target = matches!(&current, HeadState::Branch(b) if b == target_branch);

        if already_on_target {
            None
        } else {
            let current_desc = match &current {
                HeadState::Branch(b) => b.clone(),
                HeadState::Detached(sha) => format!("detached at {}", &sha[..sha.len().min(8)]),
            };
            eprintln!(
                "checking out `{}` (was on `{}`) ...",
                target_branch, current_desc
            );
            git::checkout(repo_path, target_branch).await?;
            Some(current)
        }
    } else {
        None
    };

    // Run the post-checkout body inside an inner async block so we can run
    // `restore_head` on every exit path (including the `?` errors below)
    // before propagating the result. A Drop guard would not work here
    // because restoration is async.
    let result = run_after_checkout(RunAfterCheckout {
        pkg: &pkg,
        question,
        harness_name,
        harness_config,
        effective_model,
        timeout,
        no_pull,
        effective_branch_some: effective_branch.is_some(),
        context,
        repo_path,
        db_path: db_path.clone(),
        conn,
    })
    .await;

    // Restore the original HEAD (if we changed it) regardless of how the
    // post-checkout body finished. Failures are logged but never override
    // the inner result, since the harness has already produced its answer.
    if let Some(head) = original_head {
        restore_head(repo_path, &head).await;
    }

    result
}

/// Inputs for the post-checkout phase of `run`.
///
/// Bundled into a struct so the helper signature stays readable — `run` itself
/// uses these to spawn the harness, persist the log, and update the history
/// index after a successful (or no-op) `--branch` checkout.
struct RunAfterCheckout<'a> {
    pkg: &'a Package,
    question: &'a str,
    harness_name: String,
    harness_config: crate::config::HarnessConfig,
    effective_model: Option<String>,
    timeout: u64,
    no_pull: bool,
    /// True when an effective branch (CLI override or pinned `pkg.source_branch`)
    /// was resolved for this run. Drives the auto-pull decision so the user
    /// gets the latest commits on the branch they asked about.
    effective_branch_some: bool,
    context: u32,
    repo_path: &'a Path,
    db_path: std::path::PathBuf,
    conn: rusqlite::Connection,
}

/// The post-checkout body of `kcl ask`: pull, build prompt, run harness, log.
///
/// Extracted so `run` can guarantee `restore_head` runs on every exit path
/// from this function (including any propagated `?` errors).
async fn run_after_checkout(args: RunAfterCheckout<'_>) -> Result<i32> {
    let RunAfterCheckout {
        pkg,
        question,
        harness_name,
        harness_config,
        effective_model,
        timeout,
        no_pull,
        effective_branch_some,
        context,
        repo_path,
        db_path,
        conn,
    } = args;

    // 3b. Auto-pull if applicable. --no-pull always wins. When an effective
    //     branch was resolved (either via `--branch` or via the package's
    //     pinned `source_branch`) we default to pulling so the user gets
    //     the latest commits on that branch; `--no-pull` still suppresses it.
    let should_pull = if no_pull {
        false
    } else if effective_branch_some {
        true
    } else {
        pkg.auto_pull && pkg.source_type == SourceType::Git
    };
    let mut pull_error: Option<String> = None;
    if should_pull && pkg.source_type == SourceType::Git {
        eprintln!("pulling {} ...", pkg.identifier);
        match git::pull(repo_path).await {
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

    // Release the DB connection before the long-running harness invocation so
    // it doesn't hold WAL locks (or `busy_timeout` slots) while other `kcl`
    // processes try to write.
    drop(conn);

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
    // `exit_code = None` means the child had no exit status (killed by a signal)
    // or kcl could not obtain one (spawn failure, timeout, interrupted wait).
    let (response_text, exit_code) = match harness_result {
        Ok(output) => (output.stdout, output.exit_code),
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

    // Best-effort: the user has already received the harness response, so a
    // failure to persist the log should not fail the command.
    let log_path = pkg_log_dir.join(&log_filename);
    if let Err(e) = write_log_file(&pkg_log_dir, &log_path, &log) {
        eprintln!(
            "warning: failed to write conversation log to {}: {:#}",
            log_path.display(),
            e
        );
    }

    // 7. Insert conversation index row into SQLite. Reopen the connection now
    //    that the harness has finished so we don't hold it open across the run.
    //    Best-effort: the user has already received the harness response, so a
    //    failure to persist the index row should not fail the command.
    let conversation = Conversation {
        id: conv_id,
        package_id: pkg.id.clone(),
        question: question.to_string(),
        harness: harness_name,
        exit_code,
        log_path: relative_log_path,
        created_at: started_at,
    };
    match db::open(&db_path) {
        Ok(conn) => {
            if let Err(e) = conversation.insert(&conn) {
                eprintln!("warning: failed to record conversation in history: {:#}", e);
            }
        }
        Err(e) => {
            eprintln!(
                "warning: failed to reopen database to record conversation: {:#}",
                e
            );
        }
    }

    // Exit code semantics:
    //   0 → harness succeeded
    //   2 → harness terminated without a normal exit status (signal-killed,
    //       spawn failure, timeout). Distinguished from harness errors because
    //       the child did not get to report its own result.
    //   3 → harness ran to completion but exited non-zero.
    match exit_code {
        Some(0) => Ok(0),
        Some(_) => Ok(3),
        None => Ok(2),
    }
}

/// Serialize `log` and write it to `log_path`, creating `pkg_log_dir` first.
fn write_log_file(pkg_log_dir: &Path, log_path: &Path, log: &ConversationLog) -> Result<()> {
    std::fs::create_dir_all(pkg_log_dir)
        .with_context(|| format!("failed to create log directory {}", pkg_log_dir.display()))?;
    let log_json = serde_json::to_string_pretty(log)?;
    std::fs::write(log_path, log_json)
        .with_context(|| format!("failed to write conversation log to {}", log_path.display()))?;
    Ok(())
}

/// Resolve the branch `kcl ask` should check out for this run.
///
/// CLI `--branch` always wins over the package's pinned `source_branch`. If
/// neither is set, returns `None` and the package's current HEAD is left
/// untouched.
fn resolve_target_branch<'a>(
    branch_override: Option<&'a str>,
    pkg_source_branch: Option<&'a str>,
) -> Option<&'a str> {
    branch_override.or(pkg_source_branch)
}

/// Try to restore `repo_path` to its original `HeadState`.
///
/// Failures are logged to stderr but never propagated — the harness has
/// already produced its result and the user shouldn't see a successful
/// answer turn into a failed exit code just because git was unhappy.
async fn restore_head(repo_path: &Path, head: &HeadState) {
    let target_desc = match head {
        HeadState::Branch(name) => format!("branch `{}`", name),
        HeadState::Detached(sha) => format!("detached at `{}`", &sha[..sha.len().min(8)]),
    };
    eprintln!("restoring {} ...", target_desc);
    if let Err(e) = git::restore_head(repo_path, head).await {
        eprintln!("warning: failed to restore {}: {}", target_desc, e);
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
    fn resolve_target_branch_prefers_override() {
        // Construct a Package with a pinned source_branch to mirror real usage.
        let pkg = Package::new(
            "hono".to_string(),
            "Hono".to_string(),
            SourceType::Git,
            Some("https://github.com/honojs/hono.git".to_string()),
            Some("next".to_string()),
            "/tmp/hono".to_string(),
            true,
            None,
        );

        // No CLI override: fall back to the package's pinned branch.
        assert_eq!(
            resolve_target_branch(None, pkg.source_branch.as_deref()),
            Some("next")
        );

        // CLI override wins over the pinned branch.
        assert_eq!(
            resolve_target_branch(Some("main"), pkg.source_branch.as_deref()),
            Some("main")
        );

        // Neither set: returns None (current HEAD left untouched).
        assert_eq!(resolve_target_branch(None, None), None);

        // Override set, no pinned branch.
        assert_eq!(resolve_target_branch(Some("dev"), None), Some("dev"));
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
