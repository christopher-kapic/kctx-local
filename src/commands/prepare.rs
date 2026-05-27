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

/// Hard upper bound (in bytes) on the orientation map persisted by `kcl prepare`.
///
/// The map is injected into every future `kcl ask`, so an unbounded/chatty
/// harness output becomes permanent per-ask token overhead. Maps observed in
/// practice are ~1.6k–2.9k chars; 3000 bytes keeps the useful signal while
/// capping a runaway harness.
const MAX_MAP_BYTES: usize = 3000;

/// Conservatively normalize raw harness stdout into a storable orientation map.
///
/// Transformations (in order):
/// 1. Trim leading/trailing whitespace.
/// 2. If the *entire* output is wrapped in a single markdown code fence
///    (```` ``` ```` or ```` ```lang ````), strip the surrounding fence lines
///    and keep only the inner content. Only the whole-output case is handled;
///    fences mid-content are left untouched.
/// 3. Collapse runs of 3+ consecutive blank lines down to a single blank line.
///
/// Intentionally conservative: it removes obvious wrapper noise only and never
/// attempts prose stripping or section validation, so real content is never
/// dropped. Length capping is applied separately by the caller.
fn sanitize_map(raw: &str) -> String {
    let trimmed = raw.trim();

    // Step 2: strip a single fence that wraps the entire output.
    let unfenced = {
        let mut lines: Vec<&str> = trimmed.lines().collect();
        let is_fenced = lines.len() >= 2
            && lines
                .first()
                .map(|l| l.trim_start().starts_with("```"))
                .unwrap_or(false)
            && lines.last().map(|l| l.trim() == "```").unwrap_or(false);
        if is_fenced {
            lines.remove(0);
            lines.pop();
            lines.join("\n")
        } else {
            trimmed.to_string()
        }
    };

    // Step 3: collapse 3+ consecutive blank lines into a single blank line.
    let mut out: Vec<&str> = Vec::new();
    let mut blank_run = 0usize;
    for line in unfenced.lines() {
        if line.trim().is_empty() {
            blank_run += 1;
            if blank_run <= 1 {
                out.push("");
            }
        } else {
            blank_run = 0;
            out.push(line);
        }
    }

    out.join("\n").trim().to_string()
}

/// Truncate `map` so the *final* string (content + truncation marker) is at
/// most `MAX_MAP_BYTES` bytes, cutting at a valid UTF-8 char boundary. Returns
/// the input unchanged when it already fits.
///
/// Marker space is reserved *before* slicing: appending the marker after
/// slicing to the full cap would push the stored/injected map past
/// `MAX_MAP_BYTES`, defeating the per-ask overhead bound the cap exists to
/// enforce.
fn cap_map(map: String) -> String {
    if map.len() <= MAX_MAP_BYTES {
        return map;
    }
    let marker = format!(
        "\n\n... [orientation map truncated by kcl at {} bytes]",
        MAX_MAP_BYTES
    );
    // Reserve room for the marker so content + marker stays within the cap.
    let budget = MAX_MAP_BYTES.saturating_sub(marker.len());
    let mut end = budget.min(map.len());
    while end > 0 && !map.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = map[..end].trim_end().to_string();
    capped.push_str(&marker);
    capped
}

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
        // Normalize obvious wrapper noise, then enforce a hard length cap so a
        // chatty harness can't impose unbounded per-ask overhead forever.
        let sanitized = sanitize_map(&map_content);

        // An empty/whitespace map is not a successful preparation: don't store
        // it (which would mask a real failure as a usable map). Treat it like a
        // harness failure so the process exits non-zero per the exit-code policy.
        if sanitized.is_empty() {
            eprintln!(
                "error: harness produced an empty orientation map for `{}`; nothing stored",
                pkg.identifier
            );
            return Ok(3);
        }

        let map_content = cap_map(sanitized);

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
        let branch_for_log = git_branch.as_deref().unwrap_or("global").to_string();

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

        // Migrations were already run when this command opened the DB at the
        // top of `run_prepare`; skip them on this post-harness reopen.
        match db::open_no_migrate(&db_path) {
            Ok(mut conn2) => {
                if let Err(e) = prepared.insert(&mut conn2) {
                    eprintln!("warning: failed to store prepared context: {:#}", e);
                } else {
                    eprintln!(
                        "stored prepared context for `{}` (scope: {}, branch: {})",
                        pkg.identifier, prepare_scope_at_time, branch_for_log
                    );
                }

                // Seed the outline index. The orientation map is the primary
                // deliverable, so failures here are warnings — they must not
                // mask a successful prepare with an error exit code.
                let root =
                    std::fs::canonicalize(repo_path).unwrap_or_else(|_| repo_path.to_path_buf());
                match crate::explore_index::index_target(
                    &mut conn2,
                    Some(pkg.id.as_str()),
                    &root,
                    |_p| {},
                ) {
                    Ok(stats) => {
                        eprintln!(
                            "indexed {} files ({} symbols, {} identifiers)",
                            stats.files_indexed, stats.symbols, stats.identifiers
                        );
                    }
                    Err(e) => {
                        eprintln!("warning: outline index build failed: {:#}", e);
                    }
                }
            }
            Err(e) => {
                eprintln!(
                    "warning: failed to reopen database for prepared context: {:#}",
                    e
                );
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
    use tokio::sync::Mutex;

    /// Serialize the prepare integration tests that mutate the process's XDG
    /// env vars (`XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_STATE_HOME`).
    /// Without this, parallel test threads race on those vars and observe
    /// each other's temp directories. Async-aware [`tokio::sync::Mutex`]
    /// because the guarded region awaits the harness subprocess.
    static XDG_TEST_LOCK: Mutex<()> = Mutex::const_new(());

    #[test]
    fn sanitize_trims_whitespace() {
        assert_eq!(sanitize_map("  \n\nPurpose: x\n\n  "), "Purpose: x");
    }

    #[test]
    fn sanitize_strips_whole_output_fence() {
        let raw = "```\nPurpose: x\n- src/: core\n```";
        assert_eq!(sanitize_map(raw), "Purpose: x\n- src/: core");
    }

    #[test]
    fn sanitize_strips_whole_output_fence_with_lang() {
        let raw = "```markdown\nPurpose: x\n```";
        assert_eq!(sanitize_map(raw), "Purpose: x");
    }

    #[test]
    fn sanitize_leaves_mid_content_fence_untouched() {
        // A fence that does not wrap the whole output must be preserved.
        let raw = "Purpose: x\n\n```rust\nfn main() {}\n```\n\nmore";
        assert_eq!(sanitize_map(raw), raw);
    }

    #[test]
    fn sanitize_collapses_blank_line_runs() {
        let raw = "a\n\n\n\n\nb";
        assert_eq!(sanitize_map(raw), "a\n\nb");
    }

    #[test]
    fn sanitize_empty_input_is_empty() {
        assert_eq!(sanitize_map(""), "");
        assert_eq!(sanitize_map("   \n\t \n  "), "");
        assert_eq!(sanitize_map("```\n\n```"), "");
    }

    #[test]
    fn cap_map_leaves_short_input_unchanged() {
        let s = "Purpose: x".to_string();
        assert_eq!(cap_map(s.clone()), s);
    }

    #[test]
    fn cap_map_truncates_at_char_boundary_with_marker() {
        // Multibyte char ('é' = 2 bytes) repeated past the cap; ensure the
        // result is valid UTF-8 (no split char) and carries the marker.
        let big = "é".repeat(MAX_MAP_BYTES); // 2 * MAX_MAP_BYTES bytes
        let capped = cap_map(big);
        assert!(capped.is_char_boundary(capped.len()));
        assert!(std::str::from_utf8(capped.as_bytes()).is_ok());
        assert!(capped.contains(&format!(
            "... [orientation map truncated by kcl at {} bytes]",
            MAX_MAP_BYTES
        )));
        // The FINAL string (content + marker) must stay within the hard cap.
        assert!(
            capped.len() <= MAX_MAP_BYTES,
            "capped len {} exceeds cap {}",
            capped.len(),
            MAX_MAP_BYTES
        );
        // And the content portion alone is necessarily within it too.
        let content_len = capped.find("\n\n... [orientation map truncated").unwrap();
        assert!(content_len <= MAX_MAP_BYTES);
    }

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
            prepared_args: vec![],
            inject_explore_toolkit: true,
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
        let cfg = Config {
            default_harness: "mock".to_string(),
            harnesses: {
                let mut m = std::collections::HashMap::new();
                m.insert("mock".to_string(), make_mock_harness());
                m
            },
            ..Config::default()
        };
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
            if make_git {
                Some("https://example.com/fake.git".to_string())
            } else {
                None
            },
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
        // Serialize all prepare integration tests on the shared XDG env vars.
        // Tolerate a poisoned mutex (a previous test panic) by recovering the
        // guard — the lock is only used to serialize, not to protect state.
        let _guard = XDG_TEST_LOCK.lock().await;
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

    /// Now passes because [`XDG_TEST_LOCK`] serializes the three integration
    /// tests that mutate `XDG_*` env vars — previously they raced and
    /// observed each other's temp directories.
    #[tokio::test]
    async fn prepare_stores_map_for_branch_scope_and_records_commit() {
        let _guard = XDG_TEST_LOCK.lock().await;
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

    /// See [`XDG_TEST_LOCK`] — these tests are sequential by design.
    #[tokio::test]
    async fn prepare_replaces_prior_map_for_same_scope() {
        let _guard = XDG_TEST_LOCK.lock().await;
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
