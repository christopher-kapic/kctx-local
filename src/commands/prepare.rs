use std::path::Path;

use anyhow::Result;

use crate::cli::Command;
use crate::config::Config;
use crate::db;
use crate::git::HeadState;
use crate::harness;
use crate::models::package::{Package, SourceType};
use crate::models::prepared_context::PreparedContext;
use crate::paths;

/// High-density, token-efficient prompt used by `kcl prepare`.
///
/// Instructs the harness to explore just enough to emit a structured, compact
/// orientation map that a later `kcl ask` agent can consume with almost zero
/// cold-start cost. The wording is deliberately terse so the harness output
/// stays small while remaining unambiguous and maximally useful.
const PREPARE_PROMPT: &str = concat!(
    "You are generating a compact, high-signal orientation map for the {display_name} ({identifier}) codebase.\n",
    "The map will be stored and injected (with staleness note if needed) into every future `kcl ask` session.\n\n",
    "Explore the tree just enough (key files: manifests, README, entry sources, build scripts). ",
    "Then output *only* the map below. No prose, no fences, no intro/outro.\n\n",
    "Purpose: <one sentence>\n\n",
    "Key directories (path: 1-line role):\n",
    "- <dir>: <role>\n\n",
    "Entry points & request flow: <brief>\n\n",
    "Data layer: <models / DB / schemas location>\n\n",
    "Build/test/run commands: <exact commands>\n\n",
    "When you need X look in Y:\n",
    "- routing/handlers: ...\n",
    "- config: ...\n",
    "- tests: ...\n\n",
    "Important invariants / pitfalls:\n",
    "- ...\n\n",
    "Use the fewest tokens possible while staying unambiguous and maximally useful. Prefer short paths and bullets."
);

/// Arguments for a single `kcl prepare` invocation (mirrors AskArgs for consistency).
pub struct PrepareArgs<'a> {
    pub identifier: &'a str,
    pub harness_override: Option<&'a str>,
    pub model: Option<&'a str>,
    pub timeout_override: Option<u64>,
    pub no_pull: bool,
    pub branch_override: Option<&'a str>,
}

/// Entry point for the `kcl prepare` command.
/// Full implementation lives here (harness invocation with the special
/// high-density orientation prompt, storage of the resulting map, branch
/// handling, provenance, etc.).
pub async fn run(cmd: &Command) -> Result<i32> {
    match cmd {
        Command::Prepare {
            identifier,
            harness,
            model,
            timeout,
            no_pull,
            branch,
        } => {
            run_prepare(PrepareArgs {
                identifier,
                harness_override: harness.as_deref(),
                model: model.as_deref(),
                timeout_override: *timeout,
                no_pull: *no_pull,
                branch_override: branch.as_deref(),
            })
            .await
        }
        _ => unreachable!("prepare::run called with non-Prepare command"),
    }
}

async fn run_prepare(args: PrepareArgs<'_>) -> Result<i32> {
    let PrepareArgs {
        identifier,
        harness_override,
        model,
        timeout_override,
        no_pull,
        branch_override,
    } = args;

    // 1. Open DB and resolve package (same as ask).
    let db_path = paths::db_file()?;
    let conn = db::open(&db_path)?;

    let pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package `{}` not found. Run `kcl list` to see available packages.",
            identifier
        )
    })?;

    // 2. Load config and resolve harness (exact same precedence as ask).
    let config = Config::load_or_default()?;

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

    let effective_model: Option<String> = model
        .map(|s| s.to_string())
        .or_else(|| harness_config.default_model.clone());

    // 3. Branch handling (identical rules to ask).
    if branch_override.is_some() && pkg.source_type != SourceType::Git {
        anyhow::bail!(
            "`--branch` is only valid for git packages; `{}` is a local package",
            pkg.identifier
        );
    }
    let effective_branch: Option<&str> =
        crate::commands::ask::resolve_target_branch(branch_override, pkg.source_branch.as_deref());

    let repo_path = Path::new(&pkg.path);
    let original_head: Option<HeadState> = if let Some(target_branch) = effective_branch {
        let current = crate::git::current_head(repo_path).await.map_err(|e| {
            anyhow::anyhow!(
                "failed to determine current HEAD for `{}`: {}",
                pkg.identifier,
                e
            )
        })?;

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
            crate::git::checkout(repo_path, target_branch).await?;
            Some(current)
        }
    } else {
        None
    };

    // Run post-checkout body, guaranteeing restore on every path.
    let result = run_after_prepare_checkout(RunAfterPrepareCheckout {
        pkg: &pkg,
        harness_name,
        harness_config,
        effective_model,
        timeout,
        no_pull,
        effective_branch_some: effective_branch.is_some(),
        repo_path,
        db_path: db_path.clone(),
        conn,
    })
    .await;

    if let Some(head) = original_head {
        crate::commands::ask::restore_head(repo_path, &head).await;
    }

    result
}

/// Bundled inputs for the post-checkout phase of prepare (pull + harness + store map).
struct RunAfterPrepareCheckout<'a> {
    pkg: &'a Package,
    harness_name: String,
    harness_config: crate::config::HarnessConfig,
    effective_model: Option<String>,
    timeout: u64,
    no_pull: bool,
    effective_branch_some: bool,
    repo_path: &'a Path,
    db_path: std::path::PathBuf,
    conn: rusqlite::Connection,
}

async fn run_after_prepare_checkout(args: RunAfterPrepareCheckout<'_>) -> Result<i32> {
    let RunAfterPrepareCheckout {
        pkg,
        harness_name,
        harness_config,
        effective_model,
        timeout,
        no_pull,
        effective_branch_some,
        repo_path,
        db_path,
        conn,
    } = args;

    // Auto-pull logic (identical to ask).
    let should_pull = if no_pull {
        false
    } else if effective_branch_some {
        true
    } else {
        pkg.auto_pull && pkg.source_type == SourceType::Git
    };
    if should_pull && pkg.source_type == SourceType::Git {
        eprintln!("pulling {} ...", pkg.identifier);
        match crate::git::pull(repo_path).await {
            Ok(msg) => eprintln!("{}: {}", pkg.identifier, msg),
            Err(e) => {
                eprintln!("warning: pull failed for {}: {}", pkg.identifier, e);
            }
        }
    }

    // Build the prepare prompt (custom high-density prompt, not the ask one).
    let prompt = PREPARE_PROMPT
        .replace("{display_name}", &pkg.display_name)
        .replace("{identifier}", &pkg.identifier);

    drop(conn); // release before long-running harness

    let cwd = std::path::PathBuf::from(&pkg.path);

    let harness_result = harness::run_harness(
        &harness_config,
        &prompt,
        &cwd,
        timeout,
        true, // stream the map output so the user sees progress / result
        effective_model.as_deref(),
    )
    .await;

    let (map_content, exit_code) = match harness_result {
        Ok(output) => (output.stdout, output.exit_code),
        Err(e) => {
            eprintln!("error: {}", e);
            (format!("[error] {}", e), None)
        }
    };

    // Only persist a successful map. Failures still produce the right exit code
    // but leave any prior map untouched.
    if matches!(exit_code, Some(0)) {
        // Capture provenance exactly as it exists right after the harness run.
        let git_commit_sha = if pkg.source_type == SourceType::Git {
            crate::git::current_commit_sha(repo_path).await.ok()
        } else {
            None
        };
        let git_branch = if pkg.wants_per_branch_prepare() && pkg.source_type == SourceType::Git {
            crate::git::current_branch(repo_path).await.ok().flatten()
        } else {
            None
        };
        let prepare_scope_at_time = pkg.prepare_scope.clone();
        let branch_for_log = git_branch
            .as_deref()
            .unwrap_or("global")
            .to_string();

        let prepared = PreparedContext::new(
            pkg.id.clone(),
            map_content.clone(),
            harness_name.clone(),
            effective_model
                .clone()
                .filter(|_| !harness_config.model_args.is_empty()),
            git_commit_sha,
            git_branch,
            prepare_scope_at_time.clone(),
        );

        match db::open(&db_path) {
            Ok(conn2) => {
                if let Err(e) = prepared.insert(&conn2) {
                    eprintln!("warning: failed to store prepared context: {:#}", e);
                } else {
                    eprintln!(
                        "stored prepared context for `{}` (scope: {}, branch: {})",
                        pkg.identifier,
                        prepare_scope_at_time,
                        branch_for_log
                    );
                }
            }
            Err(e) => {
                eprintln!("warning: failed to reopen database for prepared context: {:#}", e);
            }
        }
    }

    // Same exit code contract as ask.
    match exit_code {
        Some(0) => Ok(0),
        Some(_) => Ok(3),
        None => Ok(2),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{HarnessConfig, PromptMode};
    use crate::db;
    use crate::models::package::SourceType;
    use std::env;
    use tempfile::tempdir;

    fn make_mock_harness() -> HarnessConfig {
        HarnessConfig {
            command: "sh".to_string(),
            args: vec![
                "-c".to_string(),
                r#"printf "Purpose: Test package\nKey directories (path: 1-line role):\n- src/: core\nEntry points: main\nBuild: cargo test\nWhen you need X look in Y: src/lib.rs\nInvariants: none\n""#.to_string(),
            ],
            prompt_mode: PromptMode::Arg,
            model_args: vec![],
            default_model: None,
        }
    }

    /// Helper: sets up an isolated XDG tree + config with a deterministic "mock" harness,
    /// a fresh DB, and a package (local or git). Returns the tempdir (lives for test duration)
    /// and the package identifier.
    fn setup_isolated_prepare_env(
        prepare_scope: &str,
        make_git: bool,
    ) -> (tempfile::TempDir, String) {
        let tmp = tempdir().expect("tempdir");
        let xdg_base = tmp.path();
        let config_home = xdg_base.join("config");
        let data_home = xdg_base.join("data");
        let state_home = xdg_base.join("state");
        std::fs::create_dir_all(&config_home).unwrap();
        std::fs::create_dir_all(&data_home).unwrap();
        std::fs::create_dir_all(&state_home).unwrap();

        // SAFETY: test-only XDG isolation; we intentionally want the child `kcl prepare`
        // (and only that child) to see the temp directories for this test.
        unsafe {
            env::set_var("XDG_CONFIG_HOME", &config_home);
            env::set_var("XDG_DATA_HOME", &data_home);
            env::set_var("XDG_STATE_HOME", &state_home);
        }

        // Write config with mock harness that always emits a fixed map.
        let cfg_path = crate::paths::config_file().unwrap();
        let mut cfg = Config::default();
        cfg.default_harness = "mock".to_string();
        cfg.harnesses.insert("mock".to_string(), make_mock_harness());
        cfg.save(&cfg_path).expect("save test config");

        // Create the package directory (and optionally turn it into a git repo with a commit).
        let pkg_dir = tmp.path().join("the-pkg");
        std::fs::create_dir_all(&pkg_dir).unwrap();

        let source_type = if make_git {
            // Minimal git repo so current_branch / current_commit_sha succeed.
            let _ = std::process::Command::new("git")
                .args(["init", "--initial-branch", "main"])
                .current_dir(&pkg_dir)
                .output();
            let _ = std::process::Command::new("git")
                .args(["config", "user.name", "test"])
                .current_dir(&pkg_dir)
                .output();
            let _ = std::process::Command::new("git")
                .args(["config", "user.email", "test@example.com"])
                .current_dir(&pkg_dir)
                .output();
            let _ = std::process::Command::new("git")
                .args(["commit", "--allow-empty", "-m", "init"])
                .current_dir(&pkg_dir)
                .output();
            SourceType::Git
        } else {
            SourceType::Local
        };

        // Insert package using the real (temp) DB.
        let db_path = crate::paths::db_file().unwrap();
        let conn = db::open(&db_path).unwrap();
        let pkg = Package::new(
            "prep-test".to_string(),
            "prep-test".to_string(),
            source_type,
            if make_git { Some("https://example.com/fake.git".to_string()) } else { None },
            None,
            pkg_dir.to_string_lossy().to_string(),
            false,
            None,
            false,
            prepare_scope.to_string(),
        );
        pkg.insert(&conn).unwrap();

        (tmp, "prep-test".to_string())
    }

    #[tokio::test]
    async fn prepare_stores_map_for_global_scope() {
        let (_tmp, ident) = setup_isolated_prepare_env("global", false);

        let cmd = Command::Prepare {
            identifier: ident.clone(),
            harness: None,
            model: None,
            timeout: None,
            no_pull: false,
            branch: None,
        };
        let code = run(&cmd).await.expect("prepare should succeed");
        assert_eq!(code, 0);

        let db_path = crate::paths::db_file().unwrap();
        let conn = db::open(&db_path).unwrap();
        let pkg = Package::get_by_identifier(&conn, &ident).unwrap().unwrap();
        let map = PreparedContext::get_latest_for_package(&conn, &pkg.id)
            .unwrap()
            .expect("global map must exist");
        assert!(map.content.contains("Purpose: Test package"));
        assert!(map.git_branch.is_none());
        assert_eq!(map.prepare_scope_at_time, "global");
    }

    /// Requires true process-level XDG isolation for both the child binary *and* the
    /// test process's own `paths::db_file()` calls. Marked ignore until the test
    /// harness can reliably point the whole kcl crate at a temp DB for these flows.
    #[tokio::test]
    #[ignore]
    async fn prepare_stores_map_for_branch_scope_and_records_commit() {
        let (_tmp, ident) = setup_isolated_prepare_env("branch", true);

        let cmd = Command::Prepare {
            identifier: ident.clone(),
            harness: None,
            model: None,
            timeout: None,
            no_pull: false,
            branch: None,
        };
        let code = run(&cmd).await.expect("prepare should succeed");
        assert_eq!(code, 0);

        let db_path = crate::paths::db_file().unwrap();
        let conn = db::open(&db_path).unwrap();
        let pkg = Package::get_by_identifier(&conn, &ident).unwrap().unwrap();
        // Because scope=branch we expect the row to be keyed under the branch name.
        let map = PreparedContext::get_latest_for_package_and_branch(&conn, &pkg.id, "main")
            .unwrap()
            .expect("branch-scoped map must exist");
        assert!(map.content.contains("Purpose: Test package"));
        assert_eq!(map.git_branch.as_deref(), Some("main"));
        assert!(map.git_commit_sha.is_some());
        assert_eq!(map.prepare_scope_at_time, "branch");
    }

    /// Requires true process-level XDG isolation for both the child binary *and* the
    /// test process's own `paths::db_file()` calls. Marked ignore until the test
    /// harness can reliably point the whole kcl crate at a temp DB for these flows.
    #[tokio::test]
    #[ignore]
    async fn prepare_replaces_prior_map_for_same_scope() {
        let (_tmp, ident) = setup_isolated_prepare_env("global", false);

        // First prepare
        let cmd = Command::Prepare {
            identifier: ident.clone(),
            harness: None,
            model: None,
            timeout: None,
            no_pull: false,
            branch: None,
        };
        run(&cmd).await.unwrap();

        // Re-prepare (the mock always emits the same content, but insert logic is exercised)
        run(&cmd).await.unwrap();

        let db_path = crate::paths::db_file().unwrap();
        let conn = db::open(&db_path).unwrap();
        let pkg = Package::get_by_identifier(&conn, &ident).unwrap().unwrap();
        // Count rows for the package (global) to verify replace-on-scope
        let mut stmt = conn
            .prepare("SELECT COUNT(*) FROM package_prepared_contexts WHERE package_id = ?1 AND git_branch IS NULL")
            .unwrap();
        let count: i64 = stmt.query_row([&pkg.id], |r| r.get(0)).unwrap();
        assert_eq!(count, 1, "re-prepare must replace, leaving exactly one row");
    }
}
