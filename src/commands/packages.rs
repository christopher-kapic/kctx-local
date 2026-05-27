use std::io::Read;
use std::path::Path;

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use tokio::process::Command;

use crate::cli::PackagesCommand;
use crate::config::{Config, validate_harness_configured};
use crate::db;
use crate::git;
use crate::models::package::{Package, SourceType};
use crate::paths;

/// Current version of the export manifest format. Bump on incompatible changes.
const MANIFEST_VERSION: u32 = 1;

/// On-disk manifest format for `kcl packages export` / `import`.
#[derive(Debug, Serialize, Deserialize)]
struct ExportManifest {
    version: u32,
    packages: Vec<ExportEntry>,
}

#[derive(Debug, Serialize, Deserialize)]
struct ExportEntry {
    identifier: String,
    git: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    harness: Option<String>,
    auto_pull: bool,
    /// Whether the package was registered with `--shallow`.
    /// Defaults to false for backward-compatible manifests (old exports).
    #[serde(default)]
    shallow: bool,
    /// prepare-scope at export time ("global" or "branch").
    /// Defaults to "global" so old manifests continue to import cleanly.
    #[serde(default = "default_prepare_scope")]
    prepare_scope: String,
}

fn default_prepare_scope() -> String {
    "global".to_string()
}

/// Helper to open the database from the default location.
fn open_db() -> Result<rusqlite::Connection> {
    let db_path = paths::db_file()?;
    db::open(&db_path)
}

pub async fn run(command: &PackagesCommand) -> Result<()> {
    match command {
        PackagesCommand::List { verbose, json } => cmd_list(*verbose, *json),
        PackagesCommand::Add {
            identifier,
            path,
            git,
            branch,
            shallow,
        } => {
            cmd_add(
                identifier,
                path.as_deref(),
                git.as_deref(),
                branch.as_deref(),
                *shallow,
            )
            .await
        }
        PackagesCommand::Remove { identifier } => cmd_remove(identifier),
        PackagesCommand::Show { identifier, json } => cmd_show(identifier, *json),
        PackagesCommand::Pull { identifier, all } => cmd_pull(identifier.as_deref(), *all).await,
        PackagesCommand::Set {
            identifier,
            key,
            value,
            unset,
        } => cmd_set(identifier, key, value.as_deref(), *unset),
        PackagesCommand::Export => cmd_export(),
        PackagesCommand::Import { file } => cmd_import(file.as_deref()).await,
    }
}

fn cmd_list(verbose: bool, json: bool) -> Result<()> {
    let conn = open_db()?;
    let packages = Package::list_all(&conn)?;

    if json {
        let out = serde_json::to_string_pretty(&packages)?;
        println!("{out}");
    } else if verbose {
        for pkg in &packages {
            println!(
                "{}\t{}\t{}",
                pkg.identifier,
                pkg.source_type.as_str(),
                pkg.path,
            );
        }
    } else {
        for pkg in &packages {
            println!("{}", pkg.identifier);
        }
    }

    Ok(())
}

fn validate_identifier(id: &str) -> Result<()> {
    if id.is_empty() {
        bail!("Package identifier must not be empty");
    }
    if !id.chars().all(|c| {
        c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '/' || c == '@'
    }) {
        bail!(
            "Package identifier `{id}` contains invalid characters. \
             Only ASCII letters, digits, and the characters `-`, `_`, `.`, `/`, `@` are allowed."
        );
    }
    // The identifier is used as a filesystem subdirectory name under the
    // clone and log directories, so reject anything that could escape those
    // roots even though SQLite itself would accept it.
    if id.starts_with('/') {
        bail!("Package identifier `{id}` must not start with `/`.");
    }
    if id.ends_with('/') {
        bail!("Package identifier `{id}` must not end with `/`.");
    }
    for component in id.split('/') {
        if component.is_empty() {
            bail!("Package identifier `{id}` must not contain empty path segments (`//`).");
        }
        if component == "." || component == ".." {
            bail!("Package identifier `{id}` must not contain `.` or `..` path segments.");
        }
    }
    Ok(())
}

async fn cmd_add(
    identifier: &str,
    path: Option<&str>,
    git: Option<&str>,
    branch: Option<&str>,
    shallow: bool,
) -> Result<()> {
    validate_identifier(identifier)?;

    // Validate: must supply --path or --git (or both for tracking existing clone with remote).
    if path.is_none() && git.is_none() {
        bail!(
            "Must specify --path or --git (or both). Examples:\n  kcl packages add {identifier} --path /path/to/codebase\n  kcl packages add {identifier} --git https://github.com/user/repo.git"
        );
    }

    let conn = open_db()?;

    // Check for duplicate identifier.
    if Package::get_by_identifier(&conn, identifier)?.is_some() {
        bail!(
            "Package `{identifier}` already exists. Use `kcl packages show {identifier}` to view it or choose a different identifier."
        );
    }

    let (source_type, source_url, source_branch, resolved_path, auto_pull) = if let Some(git_url) =
        git
    {
        // Validate URL structurally for all git cases — fresh clones would
        // otherwise only fail inside git, and --path clones never run git.
        git::validate_git_url(git_url)?;
        // Git package — may or may not have an explicit --path.
        if let Some(p) = path {
            let abs = resolve_and_validate_path(p)?;
            let abs_path = Path::new(&abs);
            // The user is registering an existing on-disk repo against a URL.
            // Verify it actually is a git repo, and that its origin remote
            // matches what they passed — otherwise `kcl pull` / branch
            // operations will fail confusingly later.
            ensure_path_is_git_repo(abs_path)?;
            match check_origin_url(abs_path, git_url).await {
                OriginCheck::Match => {}
                OriginCheck::Mismatch(actual) => {
                    eprintln!(
                        "warning: path `{abs}` has origin `{actual}`, but you supplied `{git_url}`. Recording the package anyway — this may indicate a fork or mirror."
                    );
                }
                OriginCheck::NoRemote => {
                    eprintln!(
                        "warning: path `{abs}` has no `origin` remote — cannot verify it matches `{git_url}`."
                    );
                }
                OriginCheck::Error(e) => {
                    eprintln!(
                        "warning: failed to check origin URL of `{abs}`: {e}. Recording the package anyway."
                    );
                }
            }
            let recorded_branch = match branch {
                Some(b) => Some(b.to_string()),
                None => git::current_branch(abs_path).await.ok().flatten(),
            };
            (
                SourceType::Git,
                Some(git_url.to_string()),
                recorded_branch,
                abs,
                true,
            )
        } else if let Some(existing) = Package::get_by_source_url(&conn, git_url)? {
            // A previously-registered package already points at this git URL —
            // reuse its on-disk clone instead of cloning a second time. This
            // lets monorepos be registered under multiple identifiers.
            let pkg_dir = std::path::PathBuf::from(&existing.path);
            if std::fs::exists(&pkg_dir).with_context(|| {
                format!("failed to check existing clone at {}", pkg_dir.display())
            })? {
                eprintln!(
                    "reusing existing clone at {} (already registered as `{}`)",
                    existing.path, existing.identifier
                );
            } else {
                eprintln!(
                    "existing clone at {} was removed; re-cloning (originally registered as `{}`)",
                    existing.path, existing.identifier
                );
                if let Err(e) = git::clone(git_url, &pkg_dir, branch, shallow).await {
                    if pkg_dir.exists()
                        && let Err(cleanup_err) = std::fs::remove_dir_all(&pkg_dir)
                    {
                        eprintln!(
                            "warning: failed to clean up partial clone at {}: {}",
                            pkg_dir.display(),
                            cleanup_err
                        );
                    }
                    return Err(e);
                }
                eprintln!("cloned to {}", pkg_dir.display());
            }
            let recorded_branch = match branch {
                Some(b) => Some(b.to_string()),
                None => git::current_branch(&pkg_dir).await.ok().flatten(),
            };
            (
                SourceType::Git,
                Some(git_url.to_string()),
                recorded_branch,
                existing.path,
                true,
            )
        } else {
            // Clone the repo to clone_dir/<identifier>. When the user didn't
            // pass --branch we let git pick the remote's default branch
            // instead of hard-coding "main".
            let config = Config::load_or_default()?;
            let clone_dir = expand_tilde(&config.clone_dir)?;
            let pkg_dir = clone_dir.join(paths::package_storage_name(identifier));

            if pkg_dir.exists() {
                bail!(
                    "clone target already exists: {}; remove it first or use --path to track it",
                    pkg_dir.display()
                );
            }

            eprintln!("cloning {} ...", git_url);
            if let Err(e) = git::clone(git_url, &pkg_dir, branch, shallow).await {
                // Clean up partial clone directory so a retry doesn't hit
                // "clone target already exists".
                if pkg_dir.exists()
                    && let Err(cleanup_err) = std::fs::remove_dir_all(&pkg_dir)
                {
                    eprintln!(
                        "warning: failed to clean up partial clone at {}: {}",
                        pkg_dir.display(),
                        cleanup_err
                    );
                }
                return Err(e);
            }
            eprintln!("cloned to {}", pkg_dir.display());

            // Record the actual branch we ended up on (either the explicit
            // --branch value or the remote's default).
            let recorded_branch = match branch {
                Some(b) => Some(b.to_string()),
                None => git::current_branch(&pkg_dir).await.ok().flatten(),
            };

            (
                SourceType::Git,
                Some(git_url.to_string()),
                recorded_branch,
                pkg_dir.to_string_lossy().to_string(),
                true,
            )
        }
    } else {
        // Local package — --path is required (already checked above).
        let p = path.unwrap();
        let abs = resolve_and_validate_path(p)?;
        (SourceType::Local, None, None, abs, false)
    };

    let effective_shallow = shallow && matches!(source_type, SourceType::Git);
    let pkg = Package::new(
        identifier.to_string(),
        identifier.to_string(),
        source_type,
        source_url,
        source_branch,
        resolved_path,
        auto_pull,
        None,
        effective_shallow,
        "global".to_string(), // default; user can change with `kcl packages set <id> prepare-scope branch`
    );

    pkg.insert(&conn)?;
    eprintln!("added package `{identifier}`");
    Ok(())
}

/// Expand a leading `~` to the user's home directory.
///
/// Returns an error if the path starts with `~` but the home directory
/// cannot be determined (e.g. in minimal container environments).
fn expand_tilde(path: &str) -> Result<std::path::PathBuf> {
    if let Some(rest) = path.strip_prefix("~/") {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot expand `~`: home directory not found"))?;
        Ok(home.join(rest))
    } else if path == "~" {
        let home = dirs::home_dir()
            .ok_or_else(|| anyhow::anyhow!("cannot expand `~`: home directory not found"))?;
        Ok(home)
    } else {
        Ok(std::path::PathBuf::from(path))
    }
}

/// Resolve a path to absolute form and validate it exists as a directory.
fn resolve_and_validate_path(p: &str) -> Result<String> {
    let expanded = expand_tilde(p)?;
    let abs = if expanded.is_absolute() {
        expanded
    } else {
        std::env::current_dir()?.join(expanded)
    };

    if !abs.exists() {
        bail!("path does not exist: {}", abs.display());
    }
    if !abs.is_dir() {
        bail!("path is not a directory: {}", abs.display());
    }

    Ok(abs.to_string_lossy().to_string())
}

/// Verify that `abs` contains a `.git` entry. Uses `.exists()` rather than
/// `.is_dir()` because in git worktrees and submodules `.git` is a regular
/// file pointing at the real gitdir.
fn ensure_path_is_git_repo(abs: &Path) -> Result<()> {
    if !abs.join(".git").exists() {
        bail!(
            "path `{}` is not a git repository (no `.git` found)",
            abs.display()
        );
    }
    Ok(())
}

/// Result of comparing a repo's `origin` remote URL against an expected URL.
#[derive(Debug, PartialEq, Eq)]
enum OriginCheck {
    /// `origin` remote URL exactly matches the expected URL.
    Match,
    /// `origin` is configured but points at a different URL (carries the actual URL).
    Mismatch(String),
    /// No `origin` remote is configured.
    NoRemote,
    /// `git remote get-url` could not be invoked (e.g. spawn failure).
    Error(String),
}

/// Run `git -C <repo_path> remote get-url origin` and compare its output
/// against `expected_url`. Used to warn when `--git` and `--path` disagree.
async fn check_origin_url(repo_path: &Path, expected_url: &str) -> OriginCheck {
    let output = match Command::new("git")
        .arg("-C")
        .arg(repo_path)
        .arg("remote")
        .arg("get-url")
        .arg("origin")
        .output()
        .await
    {
        Ok(o) => o,
        Err(e) => return OriginCheck::Error(e.to_string()),
    };

    if !output.status.success() {
        // `git remote get-url origin` exits non-zero when origin doesn't
        // exist (and in any other failure mode). Treat all of these as
        // "no remote we can check against" rather than a hard error.
        return OriginCheck::NoRemote;
    }

    let actual = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if actual == expected_url {
        OriginCheck::Match
    } else {
        OriginCheck::Mismatch(actual)
    }
}

fn cmd_remove(identifier: &str) -> Result<()> {
    let conn = open_db()?;

    let pkg = Package::get_by_identifier(&conn, identifier)?;
    match pkg {
        Some(pkg) => {
            // Delete DB row first, then clean up on-disk artifacts.
            // If we crash after DB deletion but before disk cleanup, the
            // orphan directories are harmless and can be cleaned manually.
            // The reverse order would leave DB rows pointing at nothing.
            Package::delete(&conn, &pkg.id)?;

            // Remove the clone directory if it lives inside the configured
            // clone_dir (i.e. kcl created it) AND no surviving package row
            // still points at that same path. The latter guard matters for
            // monorepos registered under multiple identifiers: the first
            // identifier owns the on-disk clone and later ones reuse its
            // path, so removing any one of them must not orphan the others.
            let config = Config::load_or_default()?;
            let clone_dir = expand_tilde(&config.clone_dir)?;
            let pkg_path = std::path::PathBuf::from(&pkg.path);
            let inside_clone_dir = pkg_path.starts_with(&clone_dir);
            let still_referenced = Package::count_by_path(&conn, &pkg.path)? > 0;

            if inside_clone_dir && !still_referenced && pkg_path.is_dir() {
                std::fs::remove_dir_all(&pkg_path)?;
                eprintln!("deleted clone {}", pkg_path.display());
            } else if inside_clone_dir && still_referenced {
                eprintln!(
                    "kept clone {} (still referenced by other package(s))",
                    pkg_path.display()
                );
            }

            // Remove conversation log directory for this package.
            let log_dir = paths::log_dir()?;
            let pkg_log_dir = log_dir.join(paths::package_storage_name(identifier));
            if pkg_log_dir.is_dir() {
                std::fs::remove_dir_all(&pkg_log_dir)?;
                eprintln!("deleted logs {}", pkg_log_dir.display());
            }

            eprintln!("removed package `{identifier}`");
        }
        None => {
            bail!("Package `{identifier}` not found. Run `kcl list` to see available packages.");
        }
    }

    Ok(())
}

fn cmd_show(identifier: &str, json: bool) -> Result<()> {
    let conn = open_db()?;

    let pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package `{identifier}` not found. Run `kcl list` to see available packages."
        )
    })?;

    if json {
        let out = serde_json::to_string_pretty(&pkg)?;
        println!("{out}");
    } else {
        println!("identifier:    {}", pkg.identifier);
        println!("display_name:  {}", pkg.display_name);
        println!("source_type:   {}", pkg.source_type.as_str());
        if let Some(url) = &pkg.source_url {
            println!("source_url:    {url}");
        }
        if let Some(branch) = &pkg.source_branch {
            println!("source_branch: {branch}");
        }
        println!("path:          {}", pkg.path);
        println!("auto_pull:     {}", pkg.auto_pull);
        if let Some(harness) = &pkg.harness {
            println!("harness:       {harness}");
        }
        println!("shallow:       {}", pkg.shallow);
        if pkg.shallow {
            println!(
                "               (note: history truncated; older commits/versions unavailable until `git fetch --deepen=N` inside the clone dir)"
            );
        }
        println!("prepare_scope: {}", pkg.prepare_scope);
        if pkg.prepare_scope == "branch" {
            println!(
                "               (per-branch prepared contexts; run `kcl prepare --branch <name>` to (re)generate for a specific branch)"
            );
        }
        println!("created_at:    {}", pkg.created_at);
        println!("updated_at:    {}", pkg.updated_at);
    }

    Ok(())
}

async fn cmd_pull(identifier: Option<&str>, all: bool) -> Result<()> {
    let conn = open_db()?;

    if let Some(id) = identifier {
        // Pull a single package.
        let pkg = Package::get_by_identifier(&conn, id)?.ok_or_else(|| {
            anyhow::anyhow!("Package `{id}` not found. Run `kcl list` to see available packages.")
        })?;

        if pkg.source_type != SourceType::Git {
            bail!("Package `{id}` is not a git package. Only git packages can be pulled.");
        }

        let repo_path = Path::new(&pkg.path);
        eprintln!("pulling {} ...", pkg.identifier);
        let msg = git::pull(repo_path).await?;
        eprintln!("{}: {}", pkg.identifier, msg);
    } else if all {
        // Pull all auto-pull-enabled git packages.
        let packages = Package::list_all(&conn)?;
        let mut pulled = 0;
        let mut errors = 0;

        for pkg in &packages {
            if pkg.source_type != SourceType::Git || !pkg.auto_pull {
                continue;
            }

            let repo_path = Path::new(&pkg.path);
            eprintln!("pulling {} ...", pkg.identifier);
            match git::pull(repo_path).await {
                Ok(msg) => {
                    eprintln!("{}: {}", pkg.identifier, msg);
                    pulled += 1;
                }
                Err(e) => {
                    eprintln!("{}: error: {}", pkg.identifier, e);
                    errors += 1;
                }
            }
        }

        eprintln!("pulled {pulled} package(s), {errors} error(s)");
    } else {
        bail!("Specify a package identifier or use --all to pull all auto-pull packages.");
    }

    Ok(())
}

fn cmd_set(identifier: &str, key: &str, value: Option<&str>, unset: bool) -> Result<()> {
    let conn = open_db()?;

    let mut pkg = Package::get_by_identifier(&conn, identifier)?.ok_or_else(|| {
        anyhow::anyhow!(
            "Package `{identifier}` not found. Run `kcl list` to see available packages."
        )
    })?;

    match key {
        "auto-pull" => {
            if unset {
                bail!("cannot unset auto-pull; use `true` or `false`");
            }
            let val = value.ok_or_else(|| anyhow::anyhow!("missing value for auto-pull"))?;
            match val {
                "true" => pkg.auto_pull = true,
                "false" => pkg.auto_pull = false,
                other => {
                    bail!("invalid value for auto-pull: `{other}` (expected `true` or `false`)")
                }
            }
        }
        "harness" => {
            if unset {
                pkg.harness = None;
            } else {
                let val = value
                    .ok_or_else(|| anyhow::anyhow!("missing value for harness (or use --unset)"))?;
                let config = Config::load_or_default()?;
                validate_harness_configured(&config, val)?;
                pkg.harness = Some(val.to_string());
            }
        }
        "prepare-scope" => {
            if unset {
                bail!("cannot unset prepare-scope; use `global` or `branch`");
            }
            let val = value.ok_or_else(|| anyhow::anyhow!("missing value for prepare-scope"))?;
            match val {
                "global" | "branch" => pkg.prepare_scope = val.to_string(),
                other => {
                    bail!(
                        "invalid value for prepare-scope: `{other}` (expected `global` or `branch`)"
                    )
                }
            }
        }
        other => {
            bail!(
                "Unknown property `{other}`. Valid properties: auto-pull, harness, prepare-scope."
            );
        }
    }

    pkg.update(&conn)?;
    eprintln!("updated `{identifier}`");
    Ok(())
}

fn build_manifest(packages: &[Package]) -> (ExportManifest, Vec<String>) {
    let mut entries = Vec::new();
    let mut skipped_local = Vec::new();
    for pkg in packages {
        match (&pkg.source_type, pkg.source_url.as_deref()) {
            (SourceType::Git, Some(url)) => {
                entries.push(ExportEntry {
                    identifier: pkg.identifier.clone(),
                    git: url.to_string(),
                    branch: pkg.source_branch.clone(),
                    harness: pkg.harness.clone(),
                    auto_pull: pkg.auto_pull,
                    shallow: pkg.shallow,
                    prepare_scope: pkg.prepare_scope.clone(),
                });
            }
            _ => skipped_local.push(pkg.identifier.clone()),
        }
    }
    (
        ExportManifest {
            version: MANIFEST_VERSION,
            packages: entries,
        },
        skipped_local,
    )
}

fn cmd_export() -> Result<()> {
    let conn = open_db()?;
    let packages = Package::list_all(&conn)?;
    let (manifest, skipped) = build_manifest(&packages);

    for id in &skipped {
        eprintln!("skipping `{id}`: local package has no reproducible source URL");
    }

    let out = serde_json::to_string_pretty(&manifest)?;
    println!("{out}");
    Ok(())
}

fn read_manifest_input(file: Option<&str>) -> Result<String> {
    match file {
        None | Some("-") => {
            let mut s = String::new();
            std::io::stdin()
                .read_to_string(&mut s)
                .context("failed to read manifest from stdin")?;
            Ok(s)
        }
        Some(path) => std::fs::read_to_string(path)
            .with_context(|| format!("failed to read manifest from {path}")),
    }
}

fn parse_manifest(input: &str) -> Result<ExportManifest> {
    let manifest: ExportManifest =
        serde_json::from_str(input).context("failed to parse manifest as JSON")?;
    if manifest.version != MANIFEST_VERSION {
        bail!(
            "unsupported manifest version `{}` (this kcl understands version `{}`)",
            manifest.version,
            MANIFEST_VERSION
        );
    }
    Ok(manifest)
}

/// If the manifest entry pins a harness, verify it exists in `config` before
/// we attempt to clone the repo. Doing this up-front avoids the wasted I/O of
/// cloning a multi-gigabyte monorepo only to fail at the harness-write step.
fn validate_import_entry_harness(config: &Config, entry: &ExportEntry) -> Result<()> {
    if let Some(name) = entry.harness.as_deref() {
        validate_harness_configured(config, name)?;
    }
    Ok(())
}

async fn cmd_import(file: Option<&str>) -> Result<()> {
    let input = read_manifest_input(file)?;
    let manifest = parse_manifest(&input)?;

    let config = Config::load_or_default()?;
    let mut added = 0u32;
    let mut skipped = 0u32;
    let mut failed = 0u32;

    // One connection for the whole batch. Previously we re-opened (and re-ran
    // migrations) up to three times per entry — for 50+ packages that was 100+
    // `db::open` calls just to import. We close the connection before each
    // `cmd_add` (since `cmd_add` opens its own) and reopen after to apply the
    // manifest overrides.
    let mut conn = open_db()?;

    for entry in &manifest.packages {
        // Pre-check: skip identifiers already registered. This keeps import
        // idempotent so users can re-run it after partial failures.
        if Package::get_by_identifier(&conn, &entry.identifier)?.is_some() {
            eprintln!("skipping `{}`: already registered", entry.identifier);
            skipped += 1;
            continue;
        }

        // Validate the manifest's pinned harness against local config before
        // cloning. Otherwise we'd clone the repo, then fail to record the
        // harness, leaving an orphan on disk.
        if let Err(e) = validate_import_entry_harness(&config, entry) {
            eprintln!("error importing `{}`: {:#}", entry.identifier, e);
            failed += 1;
            continue;
        }

        // `cmd_add` opens its own connection (it's a top-level command), so we
        // must release ours across the call to avoid double-locking on the
        // same kcl process.
        drop(conn);
        let add_result = cmd_add(
            &entry.identifier,
            None,
            Some(&entry.git),
            entry.branch.as_deref(),
            entry.shallow, // preserves shallow flag from the exported manifest (defaults to false for old manifests)
        )
        .await;
        conn = open_db()?;

        match add_result {
            Ok(()) => {
                // cmd_add hardcodes auto_pull = true and harness = None for
                // git packages. Apply the manifest's overrides only when they
                // diverge from those defaults, and always reconcile
                // prepare_scope (defaults to global for old manifests). Both
                // happen on the same connection.
                if let Some(mut pkg) = Package::get_by_identifier(&conn, &entry.identifier)? {
                    let mut dirty = false;
                    if !entry.auto_pull || entry.harness.is_some() {
                        pkg.auto_pull = entry.auto_pull;
                        pkg.harness = entry.harness.clone();
                        dirty = true;
                    }
                    if pkg.prepare_scope != entry.prepare_scope {
                        pkg.prepare_scope = entry.prepare_scope.clone();
                        dirty = true;
                    }
                    if dirty {
                        pkg.update(&conn)?;
                    }
                }
                added += 1;
            }
            Err(e) => {
                eprintln!("error importing `{}`: {:#}", entry.identifier, e);
                failed += 1;
            }
        }
    }

    eprintln!("imported {added} package(s), skipped {skipped}, {failed} error(s)");
    if failed > 0 {
        bail!("{failed} package(s) failed to import");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db;
    use crate::models::package::{Package, SourceType};

    /// Helper: create an in-memory DB with a test package.
    fn setup() -> (rusqlite::Connection, Package) {
        let conn = db::open_memory().unwrap();
        let pkg = Package::new(
            "testpkg".to_string(),
            "testpkg".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/testpkg".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        pkg.insert(&conn).unwrap();
        (conn, pkg)
    }

    #[test]
    fn add_local_validates_path_exists() {
        // resolve_and_validate_path should fail for nonexistent paths.
        let result = resolve_and_validate_path("/nonexistent/path/12345");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("does not exist"));
    }

    #[test]
    fn add_local_validates_path_is_directory() {
        // A regular file should be rejected.
        let tmp = std::env::temp_dir().join("kcl_test_file");
        std::fs::write(&tmp, "test").unwrap();
        let result = resolve_and_validate_path(tmp.to_str().unwrap());
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not a directory"));
        std::fs::remove_file(&tmp).ok();
    }

    #[test]
    fn add_local_accepts_valid_directory() {
        let result = resolve_and_validate_path("/tmp");
        assert!(result.is_ok());
    }

    #[test]
    fn duplicate_identifier_rejected() {
        let (conn, _pkg) = setup();
        // Try inserting a package with the same identifier.
        let dup = Package::new(
            "testpkg".to_string(),
            "testpkg".to_string(),
            SourceType::Local,
            None,
            None,
            "/tmp/other".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        let result = dup.insert(&conn);
        assert!(result.is_err());
    }

    #[test]
    fn remove_nonexistent_package() {
        let conn = db::open_memory().unwrap();
        let result = Package::get_by_identifier(&conn, "nope").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn set_auto_pull() {
        let (conn, _pkg) = setup();
        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(!pkg.auto_pull);

        pkg.auto_pull = true;
        pkg.update(&conn).unwrap();

        let updated = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(updated.auto_pull);
    }

    #[test]
    fn set_harness() {
        let (conn, _pkg) = setup();
        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(pkg.harness.is_none());

        pkg.harness = Some("claude".to_string());
        pkg.update(&conn).unwrap();

        let updated = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert_eq!(updated.harness.as_deref(), Some("claude"));
    }

    #[test]
    fn unset_harness() {
        let (conn, _pkg) = setup();
        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        pkg.harness = Some("claude".to_string());
        pkg.update(&conn).unwrap();

        let mut pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        pkg.harness = None;
        pkg.update(&conn).unwrap();

        let updated = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        assert!(updated.harness.is_none());
    }

    #[test]
    fn second_git_package_with_same_url_reuses_path() {
        // Simulates `kcl packages add app-b --git <url>` after app-a already
        // exists for the same URL: lookup returns the existing path so the
        // caller can skip cloning.
        let conn = db::open_memory().unwrap();
        let url = "https://github.com/acme/monorepo.git";

        let first = Package::new(
            "app-a".to_string(),
            "app-a".to_string(),
            SourceType::Git,
            Some(url.to_string()),
            Some("main".to_string()),
            "/clones/monorepo".to_string(),
            true,
            None,
            false,
            "global".to_string(),
        );
        first.insert(&conn).unwrap();

        // The cmd_add code path queries by source_url before cloning.
        let existing = Package::get_by_source_url(&conn, url).unwrap();
        assert!(existing.is_some());
        let existing = existing.unwrap();
        assert_eq!(existing.path, "/clones/monorepo");

        // Insert the second package reusing that path.
        let second = Package::new(
            "app-b".to_string(),
            "app-b".to_string(),
            SourceType::Git,
            Some(url.to_string()),
            Some("main".to_string()),
            existing.path.clone(),
            true,
            None,
            false,
            "global".to_string(),
        );
        second.insert(&conn).unwrap();

        let app_a = Package::get_by_identifier(&conn, "app-a").unwrap().unwrap();
        let app_b = Package::get_by_identifier(&conn, "app-b").unwrap().unwrap();
        assert_eq!(app_a.path, app_b.path);
        assert_ne!(app_a.id, app_b.id);
    }

    #[test]
    fn list_json_serialization() {
        let (conn, _pkg) = setup();
        let packages = Package::list_all(&conn).unwrap();
        let json = serde_json::to_string_pretty(&packages).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_array());
        assert_eq!(parsed.as_array().unwrap().len(), 1);
        assert_eq!(parsed[0]["identifier"], "testpkg");
    }

    #[test]
    fn resolve_expands_tilde_prefix() {
        let result = resolve_and_validate_path("~").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.to_string_lossy());
    }

    #[test]
    fn validate_identifier_rejects_traversal() {
        // `..` as a path segment must be rejected even though `.` and `/`
        // are now allowed individually.
        let result = validate_identifier("../../etc/cron.d");
        assert!(result.is_err());
        assert!(validate_identifier("foo/../bar").is_err());
        assert!(validate_identifier("..").is_err());
        assert!(validate_identifier(".").is_err());
    }

    #[test]
    fn validate_identifier_rejects_empty() {
        let result = validate_identifier("");
        assert!(result.is_err());
        assert!(
            result
                .unwrap_err()
                .to_string()
                .contains("must not be empty")
        );
    }

    #[test]
    fn validate_identifier_accepts_valid() {
        assert!(validate_identifier("my-package").is_ok());
        assert!(validate_identifier("my_package").is_ok());
        assert!(validate_identifier("pkg123").is_ok());
        assert!(validate_identifier("A").is_ok());
        assert!(validate_identifier("@tanstack/example").is_ok());
        assert!(validate_identifier("github.com/user/repo").is_ok());
        assert!(validate_identifier("my.package").is_ok());
        assert!(validate_identifier("a/b/c").is_ok());
        assert!(validate_identifier("v1.2.3").is_ok());
    }

    #[test]
    fn package_storage_name_avoids_identifier_hierarchy() {
        let parent = paths::package_storage_name("@tanstack/example");
        let child = paths::package_storage_name("@tanstack/example/docs");

        assert!(!parent.contains('/'));
        assert!(!child.contains('/'));
        assert_ne!(parent, child);
        assert!(!std::path::Path::new(&child).starts_with(std::path::Path::new(&parent)));
    }

    #[test]
    fn validate_identifier_rejects_backslashes() {
        assert!(validate_identifier("foo\\bar").is_err());
    }

    #[test]
    fn validate_identifier_rejects_leading_or_trailing_slash() {
        assert!(validate_identifier("/foo").is_err());
        assert!(validate_identifier("foo/").is_err());
        assert!(validate_identifier("foo//bar").is_err());
    }

    #[test]
    fn remove_cleans_up_clone_and_log_dirs() {
        let (conn, _pkg) = setup();

        // Create temporary directories to simulate clone and log dirs.
        let tmp = std::env::temp_dir().join("kcl_remove_test");
        let clone_dir = tmp.join("clones");
        let log_dir = tmp.join("logs");
        let pkg_clone = clone_dir.join("testpkg");
        let pkg_log = log_dir.join("testpkg");

        std::fs::create_dir_all(&pkg_clone).unwrap();
        std::fs::create_dir_all(&pkg_log).unwrap();
        // Put a file in each to verify recursive removal.
        std::fs::write(pkg_clone.join("file.txt"), "clone").unwrap();
        std::fs::write(pkg_log.join("conv.json"), "log").unwrap();

        assert!(pkg_clone.is_dir());
        assert!(pkg_log.is_dir());

        // Delete from DB.
        Package::delete(&conn, &_pkg.id).unwrap();
        assert!(
            Package::get_by_identifier(&conn, "testpkg")
                .unwrap()
                .is_none()
        );

        // Simulate the disk cleanup from cmd_remove.
        if pkg_clone.is_dir() {
            std::fs::remove_dir_all(&pkg_clone).unwrap();
        }
        if pkg_log.is_dir() {
            std::fs::remove_dir_all(&pkg_log).unwrap();
        }

        assert!(!pkg_clone.exists());
        assert!(!pkg_log.exists());

        // Clean up the test root.
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn remove_preserves_clone_when_other_package_shares_path() {
        // Two packages registered for the same monorepo URL end up pointing
        // at the same on-disk clone. Removing one must not delete the clone
        // while the other still references it.
        let conn = db::open_memory().unwrap();
        let url = "https://github.com/acme/monorepo.git";
        let shared_path = "/clones/monorepo".to_string();

        let app_a = Package::new(
            "app-a".to_string(),
            "app-a".to_string(),
            SourceType::Git,
            Some(url.to_string()),
            Some("main".to_string()),
            shared_path.clone(),
            true,
            None,
            false,
            "global".to_string(),
        );
        app_a.insert(&conn).unwrap();

        let app_b = Package::new(
            "app-b".to_string(),
            "app-b".to_string(),
            SourceType::Git,
            Some(url.to_string()),
            Some("main".to_string()),
            shared_path.clone(),
            true,
            None,
            false,
            "global".to_string(),
        );
        app_b.insert(&conn).unwrap();

        // Remove app-a's DB row, then verify a surviving row still references
        // the path — which is the signal cmd_remove uses to skip disk cleanup.
        Package::delete(&conn, &app_a.id).unwrap();
        assert_eq!(Package::count_by_path(&conn, &shared_path).unwrap(), 1);

        // After removing app-b too, no references remain.
        Package::delete(&conn, &app_b.id).unwrap();
        assert_eq!(Package::count_by_path(&conn, &shared_path).unwrap(), 0);
    }

    #[test]
    fn remove_handles_missing_dirs_gracefully() {
        // When clone/log dirs don't exist, the is_dir() guard prevents errors.
        let nonexistent = std::path::PathBuf::from("/tmp/kcl_nonexistent_12345");
        assert!(!nonexistent.is_dir());
        // The cmd_remove pattern: only remove if is_dir().
        // This should not panic or error.
        if nonexistent.is_dir() {
            std::fs::remove_dir_all(&nonexistent).unwrap();
        }
    }

    #[test]
    fn expand_tilde_returns_ok_for_non_tilde_path() {
        let result = expand_tilde("/absolute/path").unwrap();
        assert_eq!(result, std::path::PathBuf::from("/absolute/path"));
    }

    #[test]
    fn expand_tilde_expands_home_prefix() {
        let result = expand_tilde("~/projects").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home.join("projects"));
    }

    #[test]
    fn expand_tilde_expands_bare_tilde() {
        let result = expand_tilde("~").unwrap();
        let home = dirs::home_dir().unwrap();
        assert_eq!(result, home);
    }

    #[test]
    fn show_json_serialization() {
        let (conn, _pkg) = setup();
        let pkg = Package::get_by_identifier(&conn, "testpkg")
            .unwrap()
            .unwrap();
        let json = serde_json::to_string_pretty(&pkg).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert!(parsed.is_object());
        assert_eq!(parsed["identifier"], "testpkg");
        assert_eq!(parsed["source_type"], "local");
        assert_eq!(parsed["auto_pull"], false);
        assert_eq!(parsed["path"], "/tmp/testpkg");
    }

    fn git_package(identifier: &str, url: &str, branch: Option<&str>) -> Package {
        Package::new(
            identifier.to_string(),
            identifier.to_string(),
            SourceType::Git,
            Some(url.to_string()),
            branch.map(String::from),
            format!("/clones/{identifier}"),
            true,
            None,
            false,
            "global".to_string(),
        )
    }

    #[test]
    fn build_manifest_skips_local_packages() {
        let local = Package::new(
            "local-pkg".to_string(),
            "local-pkg".to_string(),
            SourceType::Local,
            None,
            None,
            "/some/path".to_string(),
            false,
            None,
            false,
            "global".to_string(),
        );
        let git = git_package("axum", "https://github.com/tokio-rs/axum.git", Some("main"));

        let (manifest, skipped) = build_manifest(&[local, git]);

        assert_eq!(manifest.version, MANIFEST_VERSION);
        assert_eq!(manifest.packages.len(), 1);
        assert_eq!(manifest.packages[0].identifier, "axum");
        assert_eq!(
            manifest.packages[0].git,
            "https://github.com/tokio-rs/axum.git"
        );
        assert_eq!(manifest.packages[0].branch.as_deref(), Some("main"));
        assert!(manifest.packages[0].auto_pull);
        assert_eq!(skipped, vec!["local-pkg"]);
    }

    #[test]
    fn build_manifest_preserves_harness_and_auto_pull() {
        let mut git = git_package("axum", "https://github.com/tokio-rs/axum.git", None);
        git.harness = Some("claude".to_string());
        git.auto_pull = false;

        let (manifest, _) = build_manifest(&[git]);
        assert_eq!(manifest.packages[0].harness.as_deref(), Some("claude"));
        assert!(!manifest.packages[0].auto_pull);
        assert!(manifest.packages[0].branch.is_none());
    }

    #[test]
    fn manifest_round_trip_via_json() {
        let git = git_package(
            "kctx",
            "git@github.com:christopher-kapic/kctx.git",
            Some("master"),
        );
        let (manifest, _) = build_manifest(&[git]);

        let json = serde_json::to_string_pretty(&manifest).unwrap();
        let parsed = parse_manifest(&json).unwrap();
        assert_eq!(parsed.version, MANIFEST_VERSION);
        assert_eq!(parsed.packages.len(), 1);
        assert_eq!(parsed.packages[0].identifier, "kctx");
        assert_eq!(
            parsed.packages[0].git,
            "git@github.com:christopher-kapic/kctx.git"
        );
        assert_eq!(parsed.packages[0].branch.as_deref(), Some("master"));
    }

    #[test]
    fn manifest_omits_none_fields_in_json() {
        let git = git_package("axum", "https://github.com/tokio-rs/axum.git", None);
        let (manifest, _) = build_manifest(&[git]);

        let json = serde_json::to_string(&manifest).unwrap();
        // branch and harness are None — should not appear in serialized JSON.
        assert!(!json.contains("\"branch\""));
        assert!(!json.contains("\"harness\""));
        assert!(json.contains("\"auto_pull\""));
    }

    #[test]
    fn parse_manifest_rejects_wrong_version() {
        let bad = r#"{"version": 999, "packages": []}"#;
        let err = parse_manifest(bad).unwrap_err();
        assert!(err.to_string().contains("unsupported manifest version"));
    }

    #[test]
    fn parse_manifest_rejects_invalid_json() {
        let err = parse_manifest("{not valid json").unwrap_err();
        assert!(err.to_string().contains("failed to parse manifest"));
    }

    #[test]
    fn parse_manifest_accepts_minimal_entry() {
        // Only identifier, git, and auto_pull are required; branch and
        // harness are optional and may be omitted entirely.
        let minimal = r#"{
            "version": 1,
            "packages": [
                {"identifier": "axum", "git": "https://github.com/tokio-rs/axum.git", "auto_pull": true}
            ]
        }"#;
        let manifest = parse_manifest(minimal).unwrap();
        assert_eq!(manifest.packages.len(), 1);
        assert_eq!(manifest.packages[0].identifier, "axum");
        assert!(manifest.packages[0].branch.is_none());
        assert!(manifest.packages[0].harness.is_none());
    }

    #[test]
    fn parse_manifest_accepts_empty_packages_list() {
        let empty = r#"{"version": 1, "packages": []}"#;
        let manifest = parse_manifest(empty).unwrap();
        assert!(manifest.packages.is_empty());
    }

    #[test]
    fn validate_import_entry_harness_rejects_unknown_name() {
        use crate::config::{HarnessConfig, PromptMode};

        let mut config = Config::default();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );

        let entry = ExportEntry {
            identifier: "axum".to_string(),
            git: "https://github.com/tokio-rs/axum.git".to_string(),
            branch: None,
            harness: Some("nonexistent".to_string()),
            auto_pull: true,
            shallow: false,
            prepare_scope: "global".to_string(),
        };

        let err = validate_import_entry_harness(&config, &entry).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Unknown harness `nonexistent`"),
            "expected `Unknown harness` in error, got: {msg}"
        );
    }

    #[test]
    fn validate_import_entry_harness_accepts_known_name() {
        use crate::config::{HarnessConfig, PromptMode};

        let mut config = Config::default();
        config.harnesses.insert(
            "claude".to_string(),
            HarnessConfig {
                command: "claude".to_string(),
                args: vec![],
                prompt_mode: PromptMode::Arg,
                model_args: vec![],
                default_model: None,
                prepared_args: vec![],
                inject_explore_toolkit: true,
            },
        );

        let entry = ExportEntry {
            identifier: "axum".to_string(),
            git: "https://github.com/tokio-rs/axum.git".to_string(),
            branch: None,
            harness: Some("claude".to_string()),
            auto_pull: true,
            shallow: false,
            prepare_scope: "global".to_string(),
        };

        validate_import_entry_harness(&config, &entry).unwrap();
    }

    #[test]
    fn validate_import_entry_harness_skips_when_no_harness() {
        // Entries that don't pin a harness must pass validation regardless of
        // what's configured locally — the manifest format makes harness
        // optional, and a None entry inherits whatever default the importer
        // already has.
        let config = Config::default();
        let entry = ExportEntry {
            identifier: "axum".to_string(),
            git: "https://github.com/tokio-rs/axum.git".to_string(),
            branch: None,
            harness: None,
            auto_pull: true,
            shallow: false,
            prepare_scope: "global".to_string(),
        };
        validate_import_entry_harness(&config, &entry).unwrap();
    }

    /// Initialize a fresh repo at `dir` with one commit. Configures local
    /// `user.name` and `user.email` so the commit succeeds in CI sandboxes
    /// without a global git identity.
    async fn init_test_repo(dir: &Path) {
        std::fs::create_dir_all(dir).unwrap();
        async fn run(dir: &Path, args: &[&str]) {
            let out = tokio::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .output()
                .await
                .unwrap();
            assert!(
                out.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&out.stderr)
            );
        }
        run(dir, &["init", "--initial-branch", "main"]).await;
        run(dir, &["config", "user.email", "test@example.invalid"]).await;
        run(dir, &["config", "user.name", "Test User"]).await;
        run(dir, &["commit", "--allow-empty", "-m", "init"]).await;
    }

    #[test]
    fn ensure_path_is_git_repo_rejects_non_repo() {
        let tmp = std::env::temp_dir().join("kcl-test-ensure-non-repo");
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).unwrap();

        let err = ensure_path_is_git_repo(&tmp).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("not a git repository"),
            "unexpected error: {msg}"
        );
        assert!(msg.contains("`.git`"), "should mention `.git`: {msg}");

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn ensure_path_is_git_repo_accepts_real_repo() {
        let tmp = std::env::temp_dir().join("kcl-test-ensure-real-repo");
        let _ = std::fs::remove_dir_all(&tmp);
        init_test_repo(&tmp).await;

        assert!(ensure_path_is_git_repo(&tmp).is_ok());

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn check_origin_url_matches_when_remotes_agree() {
        let tmp = std::env::temp_dir().join("kcl-test-origin-match");
        let _ = std::fs::remove_dir_all(&tmp);
        init_test_repo(&tmp).await;

        let url = "https://example.com/repo.git";
        let out = tokio::process::Command::new("git")
            .args(["remote", "add", "origin", url])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());

        let result = check_origin_url(&tmp, url).await;
        assert_eq!(result, OriginCheck::Match);

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[tokio::test]
    async fn check_origin_url_returns_mismatch_with_actual_url() {
        let tmp = std::env::temp_dir().join("kcl-test-origin-mismatch");
        let _ = std::fs::remove_dir_all(&tmp);
        init_test_repo(&tmp).await;

        let actual = "https://example.com/repo-a.git";
        let supplied = "https://example.com/repo-b.git";
        let out = tokio::process::Command::new("git")
            .args(["remote", "add", "origin", actual])
            .current_dir(&tmp)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());

        let result = check_origin_url(&tmp, supplied).await;
        assert_eq!(result, OriginCheck::Mismatch(actual.to_string()));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn manifest_round_trip_preserves_shallow_and_prepare_scope() {
        // Regression: serializing then re-parsing a manifest must carry the
        // `shallow` and `prepare_scope` fields through unchanged, so a user
        // who shares an export can rebuild a matching set of packages on
        // another machine without losing those flags.
        let pkg_a = Package::new(
            "shallow-pkg".to_string(),
            "shallow-pkg".to_string(),
            SourceType::Git,
            Some("https://github.com/x/shallow.git".to_string()),
            Some("main".to_string()),
            "/clones/shallow-pkg".to_string(),
            true,
            None,
            true,                 // shallow
            "branch".to_string(), // per-branch prepare scope
        );
        let pkg_b = Package::new(
            "deep-pkg".to_string(),
            "deep-pkg".to_string(),
            SourceType::Git,
            Some("https://github.com/x/deep.git".to_string()),
            None,
            "/clones/deep-pkg".to_string(),
            true,
            None,
            false,                // not shallow (default)
            "global".to_string(), // default scope
        );

        let (manifest, _skipped) = build_manifest(&[pkg_a, pkg_b]);
        assert_eq!(manifest.packages.len(), 2);

        let shallow_entry = manifest
            .packages
            .iter()
            .find(|e| e.identifier == "shallow-pkg")
            .unwrap();
        assert!(shallow_entry.shallow);
        assert_eq!(shallow_entry.prepare_scope, "branch");
        let deep_entry = manifest
            .packages
            .iter()
            .find(|e| e.identifier == "deep-pkg")
            .unwrap();
        assert!(!deep_entry.shallow);
        assert_eq!(deep_entry.prepare_scope, "global");

        let json = serde_json::to_string(&manifest).unwrap();
        let reparsed = parse_manifest(&json).unwrap();
        let reparsed_shallow = reparsed
            .packages
            .iter()
            .find(|e| e.identifier == "shallow-pkg")
            .unwrap();
        assert!(reparsed_shallow.shallow);
        assert_eq!(reparsed_shallow.prepare_scope, "branch");
        let reparsed_deep = reparsed
            .packages
            .iter()
            .find(|e| e.identifier == "deep-pkg")
            .unwrap();
        assert!(!reparsed_deep.shallow);
        assert_eq!(reparsed_deep.prepare_scope, "global");
    }

    #[test]
    fn parse_manifest_defaults_missing_shallow_and_scope() {
        // Old manifests (pre-shallow, pre-prepare-scope) lack both fields; the
        // serde defaults must keep them importable as `shallow=false,
        // prepare_scope="global"` so an upgrade doesn't break stored exports.
        let old = r#"{
            "version": 1,
            "packages": [
                {"identifier": "axum", "git": "https://github.com/tokio-rs/axum.git", "auto_pull": true}
            ]
        }"#;
        let parsed = parse_manifest(old).unwrap();
        assert_eq!(parsed.packages.len(), 1);
        assert!(!parsed.packages[0].shallow);
        assert_eq!(parsed.packages[0].prepare_scope, "global");
    }

    #[tokio::test]
    async fn check_origin_url_returns_no_remote_when_origin_missing() {
        let tmp = std::env::temp_dir().join("kcl-test-origin-missing");
        let _ = std::fs::remove_dir_all(&tmp);
        init_test_repo(&tmp).await;

        let result = check_origin_url(&tmp, "https://example.com/repo.git").await;
        assert_eq!(result, OriginCheck::NoRemote);

        let _ = std::fs::remove_dir_all(&tmp);
    }
}
